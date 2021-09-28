use regex::Regex;
use std::io::prelude::*;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use std::{
    cmp::Reverse,
    collections::hash_map::DefaultHasher,
    collections::HashMap,
    env,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        mpsc::{
            self, {Receiver, Sender},
        },
        Arc, Mutex,
    },
    time::Instant,
};

use chrono::prelude::*;
use chrono::DateTime;
use crypto_msg_parser::{extract_symbol, parse_l2, parse_trade, MarketType, MessageType};
use dashmap::{DashMap, DashSet};
use flate2::write::GzEncoder;
use flate2::{read::GzDecoder, Compression};
use glob::glob;
use log::*;
use rand::Rng;
use rlimit::{setrlimit, Resource};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sysinfo::{RefreshKind, System, SystemExt};
use threadpool::ThreadPool;

#[derive(Serialize, Deserialize)]
pub struct Message {
    /// The exchange name, unique for each exchage
    pub exchange: String,
    /// Market type
    pub market_type: MarketType,
    /// Message type
    pub msg_type: MessageType,
    /// Unix timestamp in milliseconds
    pub received_at: u64,
    /// the original message
    pub json: String,
}

fn get_day(unix_timestamp: i64) -> String {
    let naive = NaiveDateTime::from_timestamp(unix_timestamp, 0);
    let datetime: DateTime<Utc> = DateTime::from_utc(naive, Utc);
    datetime.format("%Y-%m-%d").to_string()
}

// Output to a raw file and a parsed file.
#[derive(Clone)]
#[allow(clippy::type_complexity)]
struct Output(
    Arc<(
        Mutex<Box<dyn std::io::Write + Send>>,
        Mutex<Box<dyn std::io::Write + Send>>,
    )>,
);

fn is_blocked_market(market_type: MarketType) -> bool {
    market_type == MarketType::QuantoFuture
        || market_type == MarketType::QuantoSwap
        || market_type == MarketType::Move
        || market_type == MarketType::BVOL
}

/// Split a file by symbol and write to multiple files.
///
/// This function does split, dedup and parse together, and it is
/// thread-safe, each `input_file` will launch a thread.
///
/// ## Arguments:
///
/// - input_file A `.json.gz` file downloaded from AWS S3
/// - day `yyyy-MM-dd` string, all messages beyond [day-5min, day+5min] will be dropped
/// - output_dir_raw Where raw messages will be written to
/// - output_dir_parsed Where parsed messages will be written to
/// - split_files A HashMap that tracks opened files, key is `msg.symbol`, value is file of
///  `output_dir/exchange.market_type.msg_type.symbol.day.json.gz`. Each `exchange, msg_type, market_type`
///  has one `split_files` HashMap
/// - visited A HashSet for deduplication, each `exchange, msg_type, market_type` has one `visited` Hashset
fn split_file<P>(
    input_file: P,
    day: String,
    output_dir_raw: P,
    output_dir_parsed: P,
    split_files: Arc<DashMap<String, Output>>,
    visited: Arc<DashSet<u64>>,
) -> (i64, i64)
where
    P: AsRef<Path>,
{
    let file_name = input_file.as_ref().file_name().unwrap();
    let v: Vec<&str> = file_name.to_str().unwrap().split('.').collect();
    let exchange = v[0];
    let market_type_str = v[1];
    let market_type = MarketType::from_str(market_type_str).unwrap();
    let msg_type_str = v[2];
    let msg_type = MessageType::from_str(msg_type_str).unwrap();
    let f_in = std::fs::File::open(&input_file)
        .unwrap_or_else(|_| panic!("{:?} does not exist", input_file.as_ref().display()));
    let buf_reader = std::io::BufReader::new(GzDecoder::new(f_in));
    let mut total_lines = 0;
    let mut error_lines = 0;
    let re = Regex::new(r"[():.\\/]+").unwrap();
    for line in buf_reader.lines() {
        if let Ok(line) = line {
            total_lines += 1;
            if let Ok(msg) = serde_json::from_str::<Message>(&line) {
                debug_assert_eq!(msg.exchange, exchange);
                debug_assert_eq!(msg.market_type, market_type);
                debug_assert_eq!(msg.msg_type, msg_type);
                let is_new = {
                    let hashcode = {
                        let mut hasher = DefaultHasher::new();
                        msg.json.hash(&mut hasher);
                        hasher.finish()
                    };
                    visited.insert(hashcode)
                };
                if is_new {
                    if let Some(symbol) = extract_symbol(exchange, market_type, &msg.json) {
                        let output = {
                            if !split_files.contains_key(&symbol) {
                                let buf_writer_raw = {
                                    let output_file_name = format!(
                                        "{}.{}.{}.{}.{}.json.gz",
                                        exchange,
                                        market_type_str,
                                        msg_type_str,
                                        re.replace_all(&symbol, "_"),
                                        day
                                    );
                                    let f_out = std::fs::OpenOptions::new()
                                        .create(true)
                                        .write(true)
                                        .truncate(true)
                                        .open(
                                            Path::new(output_dir_raw.as_ref())
                                                .join(output_file_name),
                                        )
                                        .unwrap();
                                    std::io::BufWriter::new(GzEncoder::new(
                                        f_out,
                                        Compression::default(),
                                    ))
                                };

                                let buf_writer_parsed = {
                                    let pair =
                                        crypto_pair::normalize_pair(&symbol, exchange).unwrap();
                                    let pair = re.replace_all(&pair, "_");
                                    let output_file_name = format!(
                                        "{}.{}.{}.{}.{}.{}.json.gz",
                                        exchange,
                                        market_type_str,
                                        msg_type_str,
                                        re.replace_all(&pair, "_"),
                                        re.replace_all(&symbol, "_"),
                                        day
                                    );
                                    let f_out = std::fs::OpenOptions::new()
                                        .create(true)
                                        .write(true)
                                        .truncate(true)
                                        .open(
                                            Path::new(output_dir_parsed.as_ref())
                                                .join(output_file_name),
                                        )
                                        .unwrap();
                                    std::io::BufWriter::new(GzEncoder::new(
                                        f_out,
                                        Compression::default(),
                                    ))
                                };
                                split_files.insert(
                                    symbol.clone(),
                                    Output(Arc::new((
                                        Mutex::new(Box::new(buf_writer_raw)),
                                        Mutex::new(Box::new(buf_writer_parsed)),
                                    ))),
                                );
                            }
                            let entry = split_files.get(&symbol).unwrap();
                            entry.value().clone()
                        };
                        // raw
                        if day == get_day((msg.received_at / 1000_u64) as i64) {
                            writeln!(output.0 .0.lock().unwrap(), "{}", line).unwrap();
                        }
                        match msg.msg_type {
                            MessageType::L2Event => {
                                // Skip unsupported markets
                                if !is_blocked_market(market_type) {
                                    if let Ok(messages) = parse_l2(
                                        exchange,
                                        msg.market_type,
                                        &msg.json,
                                        Some(msg.received_at as i64),
                                    ) {
                                        for message in messages {
                                            if get_day(message.timestamp / 1000) == day {
                                                writeln!(
                                                    output.0 .1.lock().unwrap(),
                                                    "{}",
                                                    serde_json::to_string(&message).unwrap()
                                                )
                                                .unwrap();
                                            }
                                        }
                                    }
                                }
                            }
                            MessageType::Trade => {
                                if let Ok(messages) =
                                    parse_trade(&msg.exchange, msg.market_type, &msg.json)
                                {
                                    for message in messages {
                                        if get_day(message.timestamp / 1000) == day {
                                            writeln!(
                                                output.0 .1.lock().unwrap(),
                                                "{}",
                                                serde_json::to_string(&message).unwrap()
                                            )
                                            .unwrap();
                                        }
                                    }
                                }
                            }
                            _ => panic!("Unknown msg_type {}", msg.msg_type),
                        };
                    } else {
                        warn!("{}", line);
                        error_lines += 1;
                    }
                }
            } else {
                warn!("{}", line);
                error_lines += 1;
            }
        } else {
            error!("malformed file {}", input_file.as_ref().display());
            error_lines += 1;
        }
    }
    (error_lines, total_lines)
}

#[cfg(debug_assertions)]
fn get_memary_usage(lines: &[(i64, String)]) -> i64 {
    let mut memary_usage = 0_i64;
    for (timestamp, line) in lines {
        memary_usage += std::mem::size_of_val(timestamp) as i64;
        memary_usage += line.len() as i64;
    }
    memary_usage
}

fn sort_file<P>(
    input_file: P,
    output_file: P,
    use_pixz: bool,
    available_memory: Arc<AtomicI64>,
) -> (i64, i64)
where
    P: AsRef<Path>,
{
    assert!(input_file.as_ref().to_str().unwrap().ends_with(".json.gz"));
    assert!(output_file.as_ref().to_str().unwrap().ends_with(".json.xz"));
    if !input_file.as_ref().exists() {
        panic!("{:?} does not exist", input_file.as_ref().display());
    }

    let estimated_memory_usage = {
        let filesize = std::fs::metadata(input_file.as_ref()).unwrap().len();
        (filesize * 5) as i64
    };

    let f_in = std::fs::File::open(&input_file).unwrap();
    let buf_reader = std::io::BufReader::new(GzDecoder::new(f_in));
    let mut total_lines = 0;
    let mut error_lines = 0;
    let mut lines: Vec<(i64, String)> = Vec::new();
    for line in buf_reader.lines() {
        if let Ok(line) = line {
            total_lines += 1;
            if let Ok(msg) = serde_json::from_str::<HashMap<String, Value>>(&line) {
                if msg.contains_key("received_at") || msg.contains_key("timestamp") {
                    let timestamp = if msg.contains_key("received_at") {
                        msg.get("received_at").unwrap().as_i64().unwrap()
                    } else {
                        msg.get("timestamp").unwrap().as_i64().unwrap()
                    };
                    lines.push((timestamp, line))
                } else {
                    warn!("{}", line);
                    error_lines += 1;
                }
            } else {
                warn!("{}", line);
                error_lines += 1;
            }
        } else {
            error!("malformed file {}", input_file.as_ref().display());
            error_lines += 1;
        }
    }
    {
        #[cfg(debug_assertions)]
        debug!(
            "file {} size {}, estimated memory usage: {}, real memory usage: {}",
            input_file.as_ref().display(),
            std::fs::metadata(input_file.as_ref()).unwrap().len(),
            estimated_memory_usage,
            get_memary_usage(&lines)
        );
    }
    if error_lines == 0 {
        lines.sort_by_key(|x| x.0); // sort by timestamp

        if !use_pixz {
            let mut writer = {
                let f_out = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(output_file)
                    .unwrap();
                let e = xz2::write::XzEncoder::new(f_out, 9);
                std::io::BufWriter::new(e)
            };
            for line in lines {
                writeln!(writer, "{}", line.1).unwrap();
            }
            writer.flush().unwrap();
        } else {
            let json_file = {
                let output_dir = output_file.as_ref().parent().unwrap().to_path_buf();
                let filename = output_file.as_ref().file_name().unwrap().to_str().unwrap();
                output_dir.join(&filename[..filename.len() - 3])
            };
            let mut writer = {
                let f_out = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(json_file.as_path())
                    .unwrap();
                std::io::BufWriter::new(f_out)
            };
            for line in lines {
                writeln!(writer, "{}", line.1).unwrap();
            }
            writer.flush().unwrap();
            drop(writer);
            match std::process::Command::new("pixz")
                .args(["-9", json_file.as_path().to_str().unwrap()])
                .output()
            {
                Ok(output) => {
                    if !output.status.success() {
                        panic!("{}", String::from_utf8_lossy(&output.stderr));
                    }
                }
                Err(err) => panic!("{}", err),
            }
        }
        std::fs::remove_file(input_file).unwrap();
    }
    available_memory.fetch_add(estimated_memory_usage, Ordering::SeqCst);
    (error_lines, total_lines)
}

/// Process files of one day of the same exchange, msg_type, market_type.
///
/// Each `(exchange, msg_type, market_type, day)` will launch a process.
fn process_files_of_day(
    exchange: &str,
    msg_type: MessageType,
    market_type: MarketType,
    day: &str,
    input_dir: &str,
    output_dir_raw: &str,
    output_dir_parsed: &str,
) -> bool {
    let num_threads = num_cpus::get();
    let thread_pool = ThreadPool::new(num_threads);

    // split
    {
        let glob_pattern = format!(
            "{}/**/{}.{}.{}.{}-??-??.json.gz",
            input_dir, exchange, market_type, msg_type, day
        );
        let mut paths: Vec<PathBuf> = glob(&glob_pattern)
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        if paths.is_empty() {
            warn!("There are no files of pattern {}", glob_pattern);
            return false;
        }
        {
            // Add addtional files of tomorrow, because there might be some messages belong to today
            let next_day_first_hour = {
                let day_timestamp =
                    DateTime::parse_from_rfc3339(format!("{}T00:00:00Z", day).as_str())
                        .unwrap()
                        .timestamp_millis()
                        / 1000;
                let next_day = NaiveDateTime::from_timestamp(day_timestamp + 24 * 3600, 0);
                let next_day: DateTime<Utc> = DateTime::from_utc(next_day, Utc);
                next_day.format("%Y-%m-%d-%H").to_string()
            };
            let mut paths_of_next_day: Vec<PathBuf> = glob(
                format!(
                    "{}/**/{}.{}.{}.{}-??.json.gz",
                    input_dir, exchange, market_type, msg_type, next_day_first_hour
                )
                .as_str(),
            )
            .unwrap()
            .filter_map(Result::ok)
            .collect();
            paths.append(&mut paths_of_next_day);
        }

        info!(
            "Started split {} {} {} {}",
            exchange, market_type, msg_type, day
        );
        #[allow(clippy::type_complexity)]
        let (tx, rx): (Sender<(i64, i64)>, Receiver<(i64, i64)>) = mpsc::channel();
        let start_timstamp = Instant::now();
        // Larger files get processed first
        paths.sort_by_cached_key(|path| Reverse(std::fs::metadata(path).unwrap().len()));
        let visited: Arc<DashSet<u64>> = Arc::new(DashSet::new());
        let split_files: Arc<DashMap<String, Output>> = Arc::new(DashMap::new());
        for path in paths {
            let file_name = path.as_path().file_name().unwrap();
            let v: Vec<&str> = file_name.to_str().unwrap().split('.').collect();
            assert_eq!(exchange, v[0]);
            let market_type_str = v[1];
            assert_eq!(market_type, MarketType::from_str(market_type_str).unwrap());
            let msg_type_str = v[2];
            assert_eq!(msg_type, MessageType::from_str(msg_type_str).unwrap());
            let output_dir_raw = Path::new(output_dir_raw)
                .join(msg_type_str)
                .join(exchange)
                .join(market_type_str);
            std::fs::create_dir_all(output_dir_raw.as_path()).unwrap();
            let output_dir_parsed = Path::new(output_dir_parsed)
                .join(msg_type_str)
                .join(exchange)
                .join(market_type_str);
            std::fs::create_dir_all(output_dir_parsed.as_path()).unwrap();

            let visited_clone = visited.clone();
            let split_files_clone = split_files.clone();
            let day_clone = day.to_string();
            let thread_tx = tx.clone();
            thread_pool.execute(move || {
                let t = split_file(
                    path.as_path(),
                    day_clone,
                    output_dir_raw.as_path(),
                    output_dir_parsed.as_path(),
                    split_files_clone,
                    visited_clone,
                );
                thread_tx.send(t).unwrap();
            });
        }
        thread_pool.join();
        drop(tx); // drop the sender
        let mut total_lines = 0;
        let mut error_lines = 0;
        for t in rx {
            error_lines += t.0;
            total_lines += t.1;
        }
        for entry in split_files.iter() {
            let output = entry.value();
            output.0 .0.lock().unwrap().flush().unwrap();
            output.0 .1.lock().unwrap().flush().unwrap();
        }
        let error_ratio = (error_lines as f64) / (total_lines as f64);
        if error_ratio > 0.01 {
            // error ratio > 1%
            error!(
                "Failed to split {} {} {} {}, because error ratio {}/{}={}% is higher than 1% !",
                exchange,
                market_type,
                msg_type,
                day,
                error_lines,
                total_lines,
                error_ratio * 100.0
            );
            return false;
        } else {
            info!("Finished split {} {} {} {}, dropped {} malformed lines out of {} lines, time elapsed {} seconds", exchange, market_type, msg_type, day, error_lines, total_lines, start_timstamp.elapsed().as_secs());
        }
    }

    // sort by timestamp
    {
        let glob_pattern = format!(
            "{}/**/{}.{}.{}.*.{}.json.gz",
            output_dir_raw, exchange, market_type, msg_type, day
        );
        let paths_raw: Vec<PathBuf> = glob(&glob_pattern)
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        if paths_raw.is_empty() {
            warn!("There are no files of pattern {}", glob_pattern);
            return false;
        }

        let glob_pattern = format!(
            "{}/**/{}.{}.{}.*.{}.json.gz",
            output_dir_parsed, exchange, market_type, msg_type, day
        );
        let paths_parsed: Vec<PathBuf> = glob(&glob_pattern)
            .unwrap()
            .filter_map(Result::ok)
            .collect();

        let mut paths = if is_blocked_market(market_type) {
            for path in paths_parsed {
                std::fs::remove_file(path).unwrap();
            }
            paths_raw
        } else {
            paths_raw
                .into_iter()
                .chain(paths_parsed.into_iter())
                .collect()
        };

        info!(
            "Started sort {} {} {} {}",
            exchange, market_type, msg_type, day
        );
        #[allow(clippy::type_complexity)]
        let (tx, rx): (Sender<(i64, i64)>, Receiver<(i64, i64)>) = mpsc::channel();
        let start_timstamp = Instant::now();
        // Smaller files get processed first
        paths.sort_by_cached_key(|path| std::fs::metadata(path).unwrap().len());
        let percentile_90 = ((paths.len() as f64) * 0.9) as usize;
        let pixz_exists = Path::new("/usr/bin/pixz").exists();
        let available_memory = {
            let mut sys = System::new_with_specifics(RefreshKind::new().with_memory());
            sys.refresh_memory();
            let available_memory = sys.available_memory() * 1024;
            Arc::new(AtomicI64::new(available_memory as i64))
        };
        for (index, input_file) in paths.into_iter().enumerate() {
            {
                let estimated_memory_usage = {
                    let filesize = std::fs::metadata(input_file.as_path()).unwrap().len();
                    (filesize * 5) as i64
                };
                // Stop launching new threads if memory is not enough
                let mut rng = rand::thread_rng();
                let mut sys = System::new_with_specifics(RefreshKind::new().with_memory()); // for debug only
                while available_memory.load(Ordering::SeqCst) < estimated_memory_usage {
                    let millis = rng.gen_range(1000_u64..5000_u64);
                    sys.refresh_memory();
                    debug!("Available memory {} {} is less than estimated memory {}, sleeping for {} milliseconds", available_memory.load(Ordering::SeqCst), sys.available_memory() * 1024, estimated_memory_usage, millis);
                    std::thread::sleep(Duration::from_millis(millis));
                }
                available_memory.fetch_sub(estimated_memory_usage, Ordering::SeqCst);
            }
            let file_name = input_file.as_path().file_name().unwrap();
            let output_file_name = format!(
                "{}.json.xz",
                file_name
                    .to_str()
                    .unwrap()
                    .strip_suffix(".json.gz")
                    .unwrap()
            );
            let output_file = Path::new(input_file.parent().unwrap()).join(output_file_name);
            let tx_clone = tx.clone();
            let available_memory_clone = available_memory.clone();
            if pixz_exists && index >= percentile_90 {
                thread_pool.execute(move || {
                    let t = sort_file(input_file, output_file, true, available_memory_clone);
                    tx_clone.send(t).unwrap();
                });
            } else {
                thread_pool.execute(move || {
                    let t = sort_file(input_file, output_file, false, available_memory_clone);
                    tx_clone.send(t).unwrap();
                });
            }
        }
        thread_pool.join();
        drop(tx); // drop the sender
        let mut total_lines = 0;
        let mut error_lines = 0;
        for t in rx {
            error_lines += t.0;
            total_lines += t.1;
        }
        if error_lines == 0 {
            info!(
                "Finished sort {} {} {} {}, time elapsed {} seconds",
                exchange,
                market_type,
                msg_type,
                day,
                start_timstamp.elapsed().as_secs()
            );
            true
        } else {
            error!(
                "Failed to sort {} {} {} {}, found {} malformed lines out of {} lines, time elapsed {} seconds",
                exchange,
                market_type,
                msg_type,
                day,
                error_lines, total_lines,
                start_timstamp.elapsed().as_secs()
            );
            false
        }
    }
}

fn main() {
    env_logger::init();
    assert!(setrlimit(Resource::NOFILE, 4096, 4096).is_ok());

    let args: Vec<String> = env::args().collect();
    if args.len() != 8 {
        eprintln!("Usage: crypto-daily-processor <exchange> <msg_type> <market_type> <day> <input_dir> <output_dir_raw> <output_dir_parsed>");
        std::process::exit(1);
    }

    let exchange: &'static str = Box::leak(args[1].clone().into_boxed_str());

    let msg_type = MessageType::from_str(&args[2]);
    if msg_type.is_err() {
        eprintln!("Unknown msg type: {}", &args[2]);
        std::process::exit(1);
    }
    let msg_type = msg_type.unwrap();

    let market_type = MarketType::from_str(&args[3]);
    if market_type.is_err() {
        eprintln!("Unknown market type: {}", &args[3]);
        std::process::exit(1);
    }
    let market_type = market_type.unwrap();

    let day: &'static str = Box::leak(args[4].clone().into_boxed_str());
    let re = Regex::new(r"^\d{4}-\d{2}-\d{2}$").unwrap();
    if !re.is_match(day) {
        eprintln!("{} is invalid, day should be yyyy-MM-dd", day);
        std::process::exit(1);
    }

    let input_dir: &'static str = Box::leak(args[5].clone().into_boxed_str());
    if !Path::new(input_dir).is_dir() {
        eprintln!("{} does NOT exist", input_dir);
        std::process::exit(1);
    }
    let output_dir_raw: &'static str = Box::leak(args[6].clone().into_boxed_str());
    let output_dir_parsed: &'static str = Box::leak(args[7].clone().into_boxed_str());
    std::fs::create_dir_all(Path::new(output_dir_raw)).unwrap();
    std::fs::create_dir_all(Path::new(output_dir_parsed)).unwrap();

    if !process_files_of_day(
        exchange,
        msg_type,
        market_type,
        day,
        input_dir,
        output_dir_raw,
        output_dir_parsed,
    ) {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod test {
    use regex::Regex;

    #[test]
    fn test_clean_symbol() {
        let symbol = "a(b)c:d.e/f";
        let re = Regex::new(r"[():.\\/]+").unwrap();
        let cleaned = re.replace_all(symbol, "_");
        assert_eq!("a_b_c_d_e_f", cleaned);
    }
}
