use amazon_qldb_driver::retry;
use amazon_qldb_driver::{ion_compat, BlockingQldbDriver, QldbDriverBuilder};
use async_trait::async_trait;

use ion_c_sys::reader::IonCReaderHandle;
use ion_c_sys::result::IonCError;
use itertools::Itertools;
use rusoto_core::{
    credential::{ChainProvider, ProfileProvider, ProvideAwsCredentials},
    Region,
};
use std::error::Error as StdError;
use thiserror::Error;

use std::str::FromStr;
#[macro_use]
extern crate log;

use rustyline::error::ReadlineError;

use crate::ui::Ui;
use structopt::StructOpt;
use std::mem::zeroed;

mod repl_helper;
mod ui;

#[derive(Debug, StructOpt)]
#[structopt(
    name = "qldb-shell",
    about = "A shell for interacting with Amazon QLDB."
)]
struct Opt {
    #[structopt(short, long = "--region")]
    region: Option<String>,

    #[structopt(short, long = "--ledger")]
    ledger: String,

    #[structopt(short = "-s", long = "--qldb-session-endpoint")]
    qldb_session_endpoint: Option<String>,

    #[structopt(short, long = "--profile")]
    profile: Option<String>,

    #[structopt(short, long = "--verbose")]
    verbose: bool,

    #[structopt(short, long = "--format", default_value = "ion")]
    format: FormatMode,

    #[structopt(short, long = "--execute")]
    execute: Option<ExecuteStatementOpt>,
}

#[derive(Debug)]
enum FormatMode {
    Ion,
    Json,
}

#[derive(Error, Debug)]
enum ParseFormatModeErr {
    #[error("{0} is not a valid format mode")]
    InvalidFormatMode(String),
}

impl FromStr for FormatMode {
    type Err = ParseFormatModeErr;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match &s.to_lowercase()[..] {
            "ion" | "ion-text" => FormatMode::Ion,
            "json" => todo!("json is not yet supported"),
            _ => return Err(ParseFormatModeErr::InvalidFormatMode(s.into())),
        })
    }
}

#[derive(Debug)]
enum ExecuteStatementOpt {
    SingleStatement(String),
    Stdin,
}

impl FromStr for ExecuteStatementOpt {
    type Err = String; // never happens

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "-" => ExecuteStatementOpt::Stdin,
            _ => ExecuteStatementOpt::SingleStatement(s.into()),
        })
    }
}

pub fn run() -> Result<(), Box<dyn StdError>> {
    let opt = Opt::from_args();
    configure_logging(&opt)?;
    Runner::new(opt)?.run()
}

fn configure_logging(opt: &Opt) -> Result<(), log::SetLoggerError> {
    let level = match opt.verbose {
        true => log::LevelFilter::Debug,
        false => log::LevelFilter::Info,
    };
    fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "{}[{}][{}] {}",
                chrono::Local::now().format("[%Y-%m-%d][%H:%M:%S]"),
                record.target(),
                record.level(),
                message
            ))
        })
        .level(level)
        .chain(std::io::stdout())
        .filter(|metadata| metadata.target() != "rustyline")
        .apply()
}

/// Required for static dispatch of [`QldbSessionClient::new_with`].
enum CredentialProvider {
    Profile(ProfileProvider),
    Chain(ChainProvider),
}

#[async_trait]
impl ProvideAwsCredentials for CredentialProvider {
    async fn credentials(
        &self,
    ) -> Result<rusoto_core::credential::AwsCredentials, rusoto_core::credential::CredentialsError>
    {
        use CredentialProvider::*;
        match self {
            Profile(p) => p.credentials().await,
            Chain(c) => c.credentials().await,
        }
    }
}

fn profile_provider(opt: &Opt) -> Result<Option<ProfileProvider>, Box<dyn StdError>> {
    let it = match &opt.profile {
        Some(p) => {
            let mut prof = ProfileProvider::new()?;
            prof.set_profile(p);
            Some(prof)
        }
        None => None,
    };

    Ok(it)
}

// FIXME: Default region should consider what is set in the Profile.
fn rusoto_region(opt: &Opt) -> Result<Region, Box<dyn StdError>> {
    let it = match (&opt.region, &opt.qldb_session_endpoint) {
        (Some(r), Some(e)) => Region::Custom {
            name: r.to_owned(),
            endpoint: e.to_owned(),
        },
        (Some(r), None) => match Region::from_str(&r) {
            Ok(it) => it,
            Err(e) => {
                warn!("Unknown region {}: {}. If you know the endpoint, you can specify it and try again.", r, e);
                return Err(e)?;
            }
        },
        (None, Some(e)) => Region::Custom {
            name: Region::default().name().to_owned(),
            endpoint: e.to_owned(),
        },
        (None, None) => Region::default(),
    };

    Ok(it)
}

struct Deps {
    opt: Opt,
    driver: BlockingQldbDriver,
    ui: Ui,
}

impl Deps {
    fn new_with_opt(opt: Opt) -> Result<Deps, Box<dyn StdError>> {
        let provider = profile_provider(&opt)?;
        let region = rusoto_region(&opt)?;
        let creds = match provider {
            Some(p) => CredentialProvider::Profile(p),
            None => CredentialProvider::Chain(ChainProvider::new()),
        };

        // We disable transaction retries because they don't make sense. Users are entering statements, so if the tx fails they actually have to enter them again! We can't simply remember
        // their inputs and try again, as individual statements may be derived from values seen from yet other statements.
        let driver = QldbDriverBuilder::new()
            .ledger_name(&opt.ledger)
            .region(region)
            .credentials_provider(creds)
            .transaction_retry_policy(retry::never())
            .build()?
            .into_blocking()?;

        let ui = match opt.execute {
            Some(ref e) => {
                let reader = match e {
                    ExecuteStatementOpt::SingleStatement(statement) => statement,
                    _ => todo!(),
                };
                Ui::new_for_script(&reader[..])?
            }
            None => Ui::new(),
        };

        Ok(Deps { opt, driver, ui })
    }
}

struct Runner {
    deps: Option<Deps>,
}

impl Runner {
    fn new(opt: Opt) -> Result<Runner, Box<dyn StdError>> {
        Ok(Runner {
            deps: Some(Deps::new_with_opt(opt)?),
        })
    }

    fn run(&mut self) -> Result<(), Box<dyn StdError>> {
        let mode = IdleMode::new(self.deps.take().unwrap());
        let deps = mode.run()?.deps;
        self.deps.replace(deps.unwrap());
        Ok(())
    }
}

struct IdleMode {
    deps: Option<Deps>,
}

impl IdleMode {
    fn new(deps: Deps) -> IdleMode {
        IdleMode { deps: Some(deps) }
    }

    fn ui(&mut self) -> &mut Ui {
        match &mut self.deps {
            Some(deps) => &mut deps.ui,
            None => unreachable!(),
        }
    }

    fn run(mut self) -> Result<Self, Box<dyn StdError>> {
        println!(
            r#"Welcome to the Amazon QLDB Shell!

To start a transaction type 'start', after which you may enter a series of PartiQL statements.
When your transaction is complete, enter 'commit' or 'abort' as appropriate."#
        );

        let mndlbrt = unsafe {std::str::from_utf8_unchecked(&[0x5cu8, 0x6du8, 0x61u8, 0x6eu8, 0x64u8, 0x65u8, 0x6cu8, 0x62u8, 0x72u8, 0x6fu8, 0x74u8]) };

        loop {
            self.ui().set_prompt(format!("qldb> "));
            let user_input = self.ui().user_input();
            match user_input {
                Ok(line) => {
                    match &line[..] {
                        "help" | "HELP" | "?" => {
                            println!("To start a transaction enter 'start'. To exit, enter 'exit' or press CTRL-D.");
                        }
                        "start" | "START" => {
                            let mode = TransactionMode::new(self.deps.take().unwrap());
                            let deps = mode.run();
                            self.deps.replace(deps);
                        }
                        "quit" | "exit" | "QUIT" | "EXIT" => {
                            break;
                        }
                        _ if line.eq(mndlbrt) => {
                            Mndlbrt::plot()
                        }
                        _ => {
                            println!("unknown command");
                        }
                    };
                }
                Err(ReadlineError::Interrupted) => {
                    println!("CTRL-C");
                }
                Err(ReadlineError::Eof) => {
                    println!("CTRL-D");
                    break;
                }
                Err(err) => {
                    warn!("Error: {:?}", err);
                    break;
                }
            }
        }

        Ok(self)
    }
}

struct TransactionMode {
    deps: Option<Deps>,
}

impl TransactionMode {
    fn new(deps: Deps) -> TransactionMode {
        TransactionMode { deps: Some(deps) }
    }

    fn run(mut self) -> Deps {
        enum Outcome {
            Commit,
            Abort,
        }

        let deps = self.deps.take().unwrap();
        let Deps { opt, driver, ui } = deps;
        let committed = driver.transact(|mut tx| async {
            ui.set_prompt(format!("qldb(tx: {})> ", tx.id));
            let outcome = loop {
                match ui.user_input() {
                    Ok(line) => {
                        match &line[..] {
                            "help" | "HELP" | "?" => {
                                println!("Expecting a series of PartiQL statements or one of 'commit' or 'abort'.");
                            }
                            "abort" | "ABORT" => {
                                break Outcome::Abort;
                            }
                            "commit" | "COMMIT" => {
                                break Outcome::Commit;
                            }
                            partiql => {
                                let results = tx.execute_statement(partiql).await?;

                                results
                                    .readers()
                                    .map(|r| {
                                        formatted_display(r, &opt.format)
                                    })
                                    .intersperse(",\n".to_owned())
                                    .for_each(|p|  print!("{}", p));
                                println!()
                            }
                        }
                    }
                    Err(ReadlineError::Interrupted) => {
                        debug!("CTRL-C");
                    }
                    Err(ReadlineError::Eof) => {
                        println!("CTRL-D .. aborting");
                        break Outcome::Abort;
                    }
                    Err(err) => {
                        warn!("Error: {:?}", err);
                    }
                }
            };

            match outcome {
                Outcome::Commit => tx.ok(true).await,
                Outcome::Abort => tx.abort(false).await,
            }
        });

        let deps = Deps { opt, driver, ui };

        match committed {
            Ok(true) => println!("Transaction committed!"),
            Ok(false) => println!("Transaction aborted."),
            Err(e) => {
                println!("Error during transaction: {}", e);
                deps.ui.clear_pending();
            }
        }

        deps
    }
}

fn formatted_display(result: Result<IonCReaderHandle, IonCError>, mode: &FormatMode) -> String {
    let value = match result {
        Ok(v) => v,
        Err(e) => {
            warn!(
                "unable to display document because it could not be parsed: {}",
                e
            );
            return String::new();
        }
    };

    match mode {
        FormatMode::Ion => match ion_compat::to_string_pretty(value) {
            Ok(d) => d,
            Err(e) => {
                warn!("ion formatter is not able to display this document: {}", e);
                return String::new();
            }
        },
        FormatMode::Json => {
            todo!("json is not yet supported");
        }
    }
}

struct Mndlbrt;
impl Mndlbrt {
    const CANVAS: (u32, u32) = (80, 24);
    const LIMIT: u32 = 128;
    fn plot() {
        for y in 0 .. Mndlbrt::CANVAS.1 {
            for x in 0 .. Mndlbrt::CANVAS.0 {
                let point = Mndlbrt::pixel_to_point((x, y));
                let iterations = Mndlbrt::escape_time(point);
                match iterations {
                    _ if iterations >= 20 => print!("*"),
                    _ if iterations >= 5 => print!("."),
                    _ => print!(" "),
                };
            }
            println!()
        }
    }

    fn pixel_to_point((x, y): (u32, u32)) -> (f64, f64) {
        let (x_start, x_end) = (-2.5f64, 1f64); // real plane
        let (y_start, y_end) = (-1f64, 1f64); // imaginary plane
        let (width, height) = (x_end - x_start, y_end - y_start);
        (x_start + x as f64 * width / Mndlbrt::CANVAS.0 as f64, y_end - y as f64 * height / Mndlbrt::CANVAS.1 as f64)
    }

    fn escape_time((re, im): (f64, f64)) -> u32 {
        let mut z = (0f64, 0f64);
        for i in 0..Mndlbrt::LIMIT {
            let (x, y) = z;
            if x*x + y*y >= 4f64 {
                return i;
            }
            z = (x*x - y*y + re, 2f64*x*y + im);
        }
        return Mndlbrt::LIMIT;
    }
}
