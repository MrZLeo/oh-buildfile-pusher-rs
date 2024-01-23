use chrono::DateTime;
use clap::Parser;
use log::{debug, info};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    env,
    ffi::OsStr,
    fs::{self, File},
    io::{self, Result, Write},
    path::PathBuf,
    process::Command,
};
use walkdir::WalkDir;

#[derive(Debug, Parser)]
#[command(name = "oh-updater")]
#[command(about = "Push OpenHarmony build files to a device")]
#[command(version)]
struct BuilderArg {
    #[arg(
        short = 't',
        long = "connectkey",
        help = "Connection key of the target device"
    )]
    connect_key: String,

    #[arg(
        short = 'd',
        long,
        help = "Directory containing OpenHarmony build files"
    )]
    build_dir: PathBuf,

    #[arg(
        long,
        default_value_t = String::from("packages/phone"),
        help = "Directory containing OpenHarmony build packages, which reflects the directory structure of the devic"
    )]
    build_package_dir: String,

    #[arg(
        short = 'p',
        long,
        default_value_t = false,
        help = "Push new files to device"
    )]
    push: bool,

    #[arg(long, default_value_t = false, help = "Print debug logs")]
    debug: bool,

    #[arg(
        short = 'f',
        long = "force",
        default_value_t = false,
        help = "force update"
    )]
    force_update: bool,
}

fn main() {
    // Initialize clap command
    let args = BuilderArg::parse();

    env_logger::builder()
        .filter_level(if args.debug {
            log::LevelFilter::Debug
        } else {
            log::LevelFilter::Info
        })
        .format_timestamp(None)
        .try_init()
        .unwrap();

    // Display the values
    debug!("oh builder pusher");
    debug!("Device ID: {}", args.connect_key);
    debug!("Build Directory: {}", args.build_dir.display());

    // main logic
    BuildFilePusher {
        args,
        workdir: establish_workdir().unwrap(),
        records: None,
    }
    .run()
}

const RECORD_FILE: &str = "build_record.json";

const DIRS_TO_SCAN: [&str; 21] = [
    "applications",
    "arkcompiler",
    "base",
    "build",
    "commonlibrary",
    "cpp",
    "developtools",
    "device",
    "domains",
    "drivers",
    "foundation",
    "isa",
    "kernel",
    "libpandabase",
    "out",
    "test",
    "third_party",
    "vendor",
    "communication",
    "multimedia",
    "distributedhardware",
];

fn establish_workdir() -> Result<PathBuf> {
    let xdg_conf_home = env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| {
        let home = env::var("HOME").expect("HOME not set");
        format!("{home}/.config")
    });
    let workdir = PathBuf::from(xdg_conf_home).join("hdc_push_buildfiles");
    fs::create_dir_all(&workdir)?;
    Ok(workdir)
}

#[derive(Serialize, Deserialize)]
struct Record {
    connectkey: String,
    last_modified_date: String,
}

struct BuildFilePusher {
    args: BuilderArg,
    workdir: PathBuf,
    records: Option<Vec<Record>>,
}

impl BuildFilePusher {
    fn read_records(&mut self) {
        let record_file = self.workdir.join(RECORD_FILE);
        if record_file.exists() {
            self.records = Some(
                serde_json::from_slice::<Vec<Record>>(
                    &std::fs::read(record_file).expect("record file corrupted"),
                )
                .expect("json format corrupted"),
            );
            for record in self.records.as_deref().unwrap() {
                debug!(
                    "connectkey: {}, last_modified_date: {}",
                    record.connectkey, record.last_modified_date
                )
            }
        }
    }

    fn record_entry_exists(&self, connect_key: &str) -> bool {
        match &self.records {
            Some(records) => records.iter().any(|x| x.connectkey == connect_key),
            None => false,
        }
    }

    fn record_entry(&self, connect_key: &str) -> Option<&Record> {
        match &self.records {
            Some(records) => records.iter().find(|x| x.connectkey == connect_key),
            None => None,
        }
    }

    fn record_entry_mut(&mut self, connect_key: &str) -> Option<&mut Record> {
        match &mut self.records {
            Some(records) => records.iter_mut().find(|x| x.connectkey == connect_key),
            None => None,
        }
    }

    pub fn run(&mut self) {
        debug!("Working directory: {}", self.workdir.as_path().display());

        // read or init the build record
        self.read_records();

        let latest_modified_date = match self.record_entry(&self.args.connect_key) {
            None => DateTime::<chrono::Utc>::MIN_UTC,
            Some(Record {
                connectkey: _,
                last_modified_date,
            }) => DateTime::parse_from_rfc3339(last_modified_date)
                .expect("iso time format error")
                .into(),
        };

        // scan directories and update lastest_modified_date
        let all_files: Vec<_> = DIRS_TO_SCAN
            .into_iter()
            .map(|path| self.args.build_dir.join(path))
            .filter(|path| path.exists())
            .flat_map(|path| self.get_files(path))
            .collect();

        debug!("len of all files: {}", all_files.len());

        // filter the new files by lastest_modified_date

        let new_files: Vec<_> = all_files
            .into_iter()
            .filter(|f| {
                let file_date = DateTime::<chrono::Utc>::from(
                    f.metadata()
                        .expect("open candidate file fail")
                        .modified()
                        .expect("get candidate file's modified fail"),
                );
                if self.args.force_update {
                    file_date >= latest_modified_date
                } else {
                    file_date > latest_modified_date
                }
            })
            .collect();

        debug!("len of new files: {}", new_files.len());

        // store newer timestamp
        let new_modified_dates: Vec<_> = new_files
            .iter()
            .map(|f| {
                DateTime::<chrono::Utc>::from(
                    f.metadata()
                        .expect("open candidate file fail")
                        .modified()
                        .expect("get candidate file's modified fail"),
                )
            })
            .collect();

        debug!("len of new_modified_dates: {}", new_modified_dates.len());

        // if no newer date, this variable won't be used
        let new_modified_date = new_modified_dates.into_iter().max().unwrap_or_default();

        // map files to device path
        // TODO: detect unmapped file
        // FIXME: logic error here
        // let build_file_map: HashMap<_, _> = new_files
        //     .into_iter()
        //     .flat_map(|f| self.find_device_path(f))
        //     .collect();

        let build_file_map: HashMap<_, _> = new_files
            .into_iter()
            .map(|f| {
                (
                    f.clone(),
                    self.find_device_path(f.file_name().expect("get build file name")),
                )
            })
            .collect();

        debug!("len of build file map: {}", build_file_map.len());

        // decide whether to send files
        let mut send = false;
        if !build_file_map.is_empty() && self.record_entry_exists(&self.args.connect_key) {
            info!("Found the following new files: ");
            for (build_file, device_path) in &build_file_map {
                println!("{} -> {}", build_file.display(), device_path.display());
            }
            send = self.args.push || self.decide_send_by_user();
            if send {
                Command::new("hdc")
                    .args([
                        "-t",
                        &self.args.connect_key,
                        "shell",
                        "mount",
                        "-o",
                        "remount,rw",
                        "/",
                    ])
                    .status()
                    .expect("fail to mount directory to device");

                build_file_map
                    .into_iter()
                    .for_each(|(build_file, device_path)| {
                        Command::new("hdc")
                            .args([
                                "-t",
                                &self.args.connect_key,
                                "file",
                                "send",
                                build_file.to_str().unwrap(),
                                device_path.to_str().unwrap(),
                            ])
                            .status()
                            .unwrap_or_else(|error| {
                                panic!(
                                    "fail to send {} to {}, error: {error}",
                                    build_file.display(),
                                    device_path.display()
                                )
                            });
                    });
            }
        }

        // modified records
        if send || !self.record_entry_exists(&self.args.connect_key) {
            if self.record_entry_exists(&self.args.connect_key) {
                self.record_entry_mut(&self.args.connect_key.clone())
                    .unwrap()
                    .last_modified_date = new_modified_date.to_rfc3339();
            } else {
                let records = &mut self.records;
                match records {
                    Some(ref mut r) => r.push(Record {
                        connectkey: self.args.connect_key.clone(),
                        last_modified_date: new_modified_date.to_rfc3339(),
                    }),
                    None => {
                        *records = Some(vec![Record {
                            connectkey: self.args.connect_key.clone(),
                            last_modified_date: new_modified_date.to_rfc3339(),
                        }])
                    }
                }
            }

            // update record file
            let records = serde_json::to_string(&self.records).expect("convert records to json");

            let mut record_file = File::create(self.workdir.join(RECORD_FILE))
                .expect("open record file in write-only modee");

            record_file
                .write_all(records.as_bytes())
                .expect("write json to record file");

            info!("update record files");
        }

        info!("Unchange");

        // TODO: print helper logs
        // if new_files.is_empty() && self.record_entry_exists(&self.args.connect_key) {
        //     info!("No new files since last check.");
        // } else if build_file_map.is_empty() && self.record_entry_exists(&self.args.connect_key) {
        //     info!("No pushable new files since last check.");
        // } else if !self.record_entry_exists(&self.args.connect_key) {
        //     info!(
        //         "First time running this script for device {}. Last modified date is {}.",
        //         self.args.connect_key, new_modified_date
        //     );
        // }
    }

    fn get_files(&self, path: PathBuf) -> Vec<PathBuf> {
        WalkDir::new(path)
            .into_iter()
            .filter(|f| f.as_ref().is_ok_and(|entry| entry.file_type().is_file()))
            .map(|f| f.unwrap().into_path())
            .collect()
    }

    fn find_device_path(&self, file_name: &OsStr) -> PathBuf {
        // debug!("file_name={:?}", file_name);
        let package_dir = self.args.build_dir.join(&self.args.build_package_dir);
        WalkDir::new(package_dir.as_path())
            .into_iter()
            .filter(|f| {
                f.as_ref()
                    .is_ok_and(|e| e.file_type().is_file() && e.file_name() == file_name)
            })
            .map(|f| {
                f.expect("walk device path file error")
                    .path()
                    .strip_prefix(package_dir.as_path())
                    .expect("strip build package directory error")
                    .to_path_buf()
            })
            .collect()
    }

    fn decide_send_by_user(&self) -> bool {
        print!("Do you want to proceed? [Y/n] ");
        let _ = io::stdout().flush();

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .expect("Failed to read line");

        let input = input.trim();

        matches!(input, "y" | "Y" | "")
    }
}
