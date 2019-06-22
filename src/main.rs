// Don't create a new standard console window when launched from the windows GUI.
#![windows_subsystem = "windows"]

use failure::{Error, Fallible};
use log::error;
use std::ffi::OsString;
use structopt::StructOpt;
use tabout::{tabulate_output, Alignment, Column};

use std::rc::Rc;
use std::sync::Arc;

mod config;
mod frontend;
mod mux;
mod opengl;
mod ratelim;
mod server;
use crate::frontend::FrontEndSelection;
use crate::mux::domain::{alloc_domain_id, Domain, LocalDomain};
use crate::mux::Mux;
use crate::server::client::Client;
use crate::server::domain::ClientDomain;
use portable_pty::cmdbuilder::CommandBuilder;

mod font;
use crate::font::{FontConfiguration, FontSystemSelection};

use portable_pty::PtySize;
use std::env;

/// Determine which shell to run.
/// We take the contents of the $SHELL env var first, then
/// fall back to looking it up from the password database.
#[cfg(unix)]
fn get_shell() -> Result<String, Error> {
    env::var("SHELL").or_else(|_| {
        let ent = unsafe { libc::getpwuid(libc::getuid()) };

        if ent.is_null() {
            Ok("/bin/sh".into())
        } else {
            use failure::format_err;
            use std::ffi::CStr;
            use std::str;
            let shell = unsafe { CStr::from_ptr((*ent).pw_shell) };
            shell
                .to_str()
                .map(str::to_owned)
                .map_err(|e| format_err!("failed to resolve shell: {:?}", e))
        }
    })
}

#[cfg(windows)]
fn get_shell() -> Result<String, Error> {
    Ok(env::var("ComSpec").unwrap_or("cmd.exe".into()))
}

//    let message = "; ❤ 😍🤢\n\x1b[91;mw00t\n\x1b[37;104;m bleet\x1b[0;m.";
//    terminal.advance_bytes(message);
// !=

#[derive(Debug, StructOpt)]
#[structopt(about = "Wez's Terminal Emulator\nhttp://github.com/wez/wezterm")]
#[structopt(raw(setting = "structopt::clap::AppSettings::ColoredHelp"))]
struct Opt {
    /// Skip loading ~/.wezterm.toml
    #[structopt(short = "n")]
    skip_config: bool,

    #[structopt(subcommand)]
    cmd: Option<SubCommand>,
}

#[derive(Debug, StructOpt, Default, Clone)]
struct StartCommand {
    #[structopt(
        long = "front-end",
        raw(
            possible_values = "&FrontEndSelection::variants()",
            case_insensitive = "true"
        )
    )]
    front_end: Option<FrontEndSelection>,

    #[structopt(
        long = "font-system",
        raw(
            possible_values = "&FontSystemSelection::variants()",
            case_insensitive = "true"
        )
    )]
    font_system: Option<FontSystemSelection>,

    /// If true, do not connect to domains marked as connect_automatically
    /// in your wezterm.toml configuration file.
    #[structopt(long = "no-auto-connect")]
    no_auto_connect: bool,

    /// Detach from the foreground and become a background process
    #[structopt(long = "daemonize")]
    daemonize: bool,

    /// Instead of executing your shell, run PROG.
    /// For example: `wezterm start -- bash -l` will spawn bash
    /// as if it were a login shell.
    #[structopt(parse(from_os_str))]
    prog: Vec<OsString>,
}

#[derive(Debug, StructOpt, Clone)]
enum SubCommand {
    #[structopt(name = "start", about = "Start a front-end")]
    #[structopt(raw(setting = "structopt::clap::AppSettings::ColoredHelp"))]
    Start(StartCommand),

    #[structopt(name = "cli", about = "Interact with experimental mux server")]
    #[structopt(raw(setting = "structopt::clap::AppSettings::ColoredHelp"))]
    Cli(CliCommand),
}

#[derive(Debug, StructOpt, Clone)]
struct CliCommand {
    #[structopt(subcommand)]
    sub: CliSubCommand,
}

#[derive(Debug, StructOpt, Clone)]
enum CliSubCommand {
    #[structopt(name = "list", about = "list windows and tabs")]
    #[structopt(raw(setting = "structopt::clap::AppSettings::ColoredHelp"))]
    List,
}

fn run_terminal_gui(config: Arc<config::Config>, opts: &StartCommand) -> Fallible<()> {
    #[cfg(unix)]
    {
        if opts.daemonize {
            use std::fs::OpenOptions;
            let mut options = OpenOptions::new();
            options.write(true).create(true).append(true);
            let stdout = options.open(config.daemon_options.stdout())?;
            let stderr = options.open(config.daemon_options.stderr())?;
            daemonize::Daemonize::new()
                .stdout(stdout)
                .stderr(stderr)
                .pid_file(config.daemon_options.pid_file())
                .start()?;
        }
    }

    let font_system = opts.font_system.unwrap_or(config.font_system);
    font_system.set_default();

    let fontconfig = Rc::new(FontConfiguration::new(Arc::clone(&config), font_system));

    let cmd = if !opts.prog.is_empty() {
        let argv: Vec<&std::ffi::OsStr> = opts.prog.iter().map(|x| x.as_os_str()).collect();
        let mut builder = CommandBuilder::new(&argv[0]);
        builder.args(&argv[1..]);
        Some(builder)
    } else {
        None
    };

    let domain: Arc<dyn Domain> = Arc::new(LocalDomain::new(&config)?);
    let mux = Rc::new(mux::Mux::new(&config, &domain));
    Mux::set_mux(&mux);

    let front_end = opts.front_end.unwrap_or(config.front_end);
    let gui = front_end.try_new(&mux)?;
    domain.attach()?;

    fn attach_client(mux: &Rc<Mux>, client: ClientDomain) -> Fallible<()> {
        let domain: Arc<dyn Domain> = Arc::new(client);
        mux.add_domain(&domain);
        domain.attach()
    }

    if front_end != FrontEndSelection::MuxServer && !opts.no_auto_connect {
        for unix_dom in &config.unix_domains {
            if unix_dom.connect_automatically {
                let domain_id = alloc_domain_id();
                attach_client(
                    &mux,
                    ClientDomain::new(Client::new_unix_domain(domain_id, &config, unix_dom)?),
                )?;
            }
        }

        for tls_client in &config.tls_clients {
            if tls_client.connect_automatically {
                let domain_id = alloc_domain_id();
                attach_client(
                    &mux,
                    ClientDomain::new(Client::new_tls(domain_id, &config, tls_client)?),
                )?;
            }
        }
    }

    if mux.is_empty() {
        let window_id = mux.new_empty_window();
        let tab = mux
            .default_domain()
            .spawn(PtySize::default(), cmd, window_id)?;
        gui.spawn_new_window(mux.config(), &fontconfig, &tab, window_id)?;
    }

    gui.run_forever()
}

fn main() -> Result<(), Error> {
    pretty_env_logger::init();
    // This is a bit gross.
    // In order to not to automatically open a standard windows console when
    // we run, we use the windows_subsystem attribute at the top of this
    // source file.  That comes at the cost of causing the help output
    // to disappear if we are actually invoked from a console.
    // This AttachConsole call will attach us to the console of the parent
    // in that situation, but since we were launched as a windows subsystem
    // application we will be running asynchronously from the shell in
    // the command window, which means that it will appear to the user
    // that we hung at the end, when in reality the shell is waiting for
    // input but didn't know to re-draw the prompt.
    #[cfg(windows)]
    unsafe {
        winapi::um::wincon::AttachConsole(winapi::um::wincon::ATTACH_PARENT_PROCESS)
    };

    let opts = Opt::from_args();
    let config = Arc::new(if opts.skip_config {
        config::Config::default_config()
    } else {
        config::Config::load()?
    });

    match opts
        .cmd
        .as_ref()
        .cloned()
        .unwrap_or_else(|| SubCommand::Start(StartCommand::default()))
    {
        SubCommand::Start(start) => {
            error!("Using configuration: {:#?}\nopts: {:#?}", config, opts);
            run_terminal_gui(config, &start)
        }
        SubCommand::Cli(cli) => {
            let client = Client::new_default_unix_domain(&config)?;
            match cli.sub {
                CliSubCommand::List => {
                    let cols = vec![
                        Column {
                            name: "WINID".to_string(),
                            alignment: Alignment::Right,
                        },
                        Column {
                            name: "TABID".to_string(),
                            alignment: Alignment::Right,
                        },
                        Column {
                            name: "SIZE".to_string(),
                            alignment: Alignment::Left,
                        },
                        Column {
                            name: "TITLE".to_string(),
                            alignment: Alignment::Left,
                        },
                    ];
                    let mut data = vec![];
                    let tabs = client.list_tabs().wait()?;
                    for entry in tabs.tabs.iter() {
                        data.push(vec![
                            entry.window_id.to_string(),
                            entry.tab_id.to_string(),
                            format!("{}x{}", entry.size.cols, entry.size.rows),
                            entry.title.clone(),
                        ]);
                    }
                    tabulate_output(&cols, &data, &mut std::io::stdout().lock())?;
                }
            }
            Ok(())
        }
    }
}
