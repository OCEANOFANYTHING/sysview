//! `sysview service …`: hand the dashboard to the platform's own supervisor
//! (systemd, launchd or the Windows Task Scheduler), so it comes back after a
//! logout and a reboot. The supervisor owns restarts, boot order and logging —
//! no daemon loop is added here.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The service/task name, and how every supervisor refers to it.
const NAME: &str = "sysview";
const REPO: &str = "https://github.com/OCEANOFANYTHING/sysview";

/// What to install.
pub struct Options {
    /// Machine-wide rather than the current user.
    pub system: bool,
    pub bind: String,
    pub port: u16,
    pub interval: f64,
    pub max_procs: usize,
    /// The token the installed service keeps; never passed on the command
    /// line where the process table would show it.
    pub token: String,
}

impl Options {
    /// The argv every supervisor reproduces: this binary, then the same flags
    /// a human would type.
    fn argv(&self) -> Vec<String> {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from(NAME));
        let mut v = vec![exe.display().to_string()];
        v.extend([
            "--interval".to_string(),
            format!("{}", self.interval),
            "serve".to_string(),
            "--bind".to_string(),
            self.bind.clone(),
            "--port".to_string(),
            self.port.to_string(),
            "--max-procs".to_string(),
            self.max_procs.to_string(),
        ]);
        v
    }
}

/// The unit/task as it will be written, plus what to run afterwards. Keeping
/// it a value is what makes `--print` honest.
struct Plan {
    supervisor: &'static str,
    /// `None` for the Task Scheduler, which owns the definition itself.
    file: Option<PathBuf>,
    content: String,
    /// Token file, written 0600 where the platform allows it.
    secret: Option<(PathBuf, String)>,
    commands: Vec<Vec<String>>,
}

pub fn install(o: &Options, print_only: bool) -> Result<String, String> {
    let plan = platform_plan(o);
    if print_only {
        let mut s = format!("# {}: {}\n", plan.supervisor, target(&plan));
        if let Some((p, _)) = &plan.secret {
            s.push_str(&format!("# {} (mode 0600)\n", p.display()));
        }
        s.push_str(&plan.content);
        if !plan.content.ends_with('\n') {
            s.push('\n');
        }
        for cmd in &plan.commands {
            s.push_str(&format!("$ {}\n", cmd.join(" ")));
        }
        return Ok(s);
    }

    if let Some((path, body)) = &plan.secret {
        write_file(path, body, true)?;
    }
    if let Some(path) = &plan.file {
        write_file(path, &plan.content, false)?;
    }
    for cmd in &plan.commands {
        run(cmd).map_err(|e| format!("{e}{}", failure_hint(o.system)))?;
    }
    Ok(summary(o, &plan))
}

/// Why the supervisor command refused, in the user's terms. Empty when the
/// default already implies no elevation.
fn failure_hint(system: bool) -> &'static str {
    match (cfg!(windows), system) {
        (true, _) => " (schtasks needs an elevated shell)",
        (false, true) => " (a machine-wide unit needs root)",
        (false, false) => "",
    }
}

pub fn uninstall(system: bool) -> Result<String, String> {
    for cmd in uninstall_commands(system) {
        // Already-gone is not a failure: removing twice is normal.
        let _ = run(&cmd);
    }
    for path in install_files(system) {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot remove {}: {e}", path.display())),
        }
    }
    Ok(format!("{NAME}: uninstalled"))
}

pub fn status(system: bool) -> Result<(), String> {
    if let Some(path) = install_files(system).first()
        && !path.exists()
    {
        println!("{NAME}: not installed at {}", path.display());
        return Ok(());
    }
    for cmd in status_commands(system) {
        if let Some((prog, args)) = cmd.split_first() {
            let _ = Command::new(prog).args(args).status();
        }
    }
    Ok(())
}

// ------------------------------------------------------------ plumbing -----

fn target(plan: &Plan) -> String {
    plan.file
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "scheduled task".to_string())
}

fn run(cmd: &[String]) -> Result<(), String> {
    let (prog, args) = cmd.split_first().ok_or("empty command")?;
    let status = Command::new(prog)
        .args(args)
        .status()
        .map_err(|e| format!("cannot run {prog}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{prog} exited with {status}"))
    }
}

fn write_file(path: &Path, body: &str, secret: bool) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(path, body).map_err(|e| {
        let hint = if e.kind() == std::io::ErrorKind::PermissionDenied {
            " (try again as root/Administrator)"
        } else {
            ""
        };
        format!("cannot write {}: {e}{hint}", path.display())
    })?;
    #[cfg(unix)]
    if secret {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("cannot restrict {}: {e}", path.display()))?;
    }
    let _ = secret;
    Ok(())
}

fn summary(o: &Options, plan: &Plan) -> String {
    format!(
        "installed {} at {}\n  dashboard  http://{}/\n  token      {}\n  docs       {REPO}\n  manage     {}",
        plan.supervisor,
        target(plan),
        host_for_url(&o.bind, o.port),
        o.token,
        manage_hint(o.system),
    )
}

fn host_for_url(bind: &str, port: u16) -> String {
    let host = match bind {
        "0.0.0.0" | "::" | "[::]" => "localhost",
        other => other,
    };
    format!("{host}:{port}")
}

/// Quotes an argument only where a parser would need it.
fn quote(arg: &str) -> String {
    if arg.contains([' ', '"', '\'', '\\']) {
        format!("\"{}\"", arg.replace('"', "\\\""))
    } else {
        arg.to_string()
    }
}

fn command_line(argv: &[String]) -> String {
    argv.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
}

// --------------------------------------------------------------- linux -----

#[cfg(target_os = "linux")]
fn platform_plan(o: &Options) -> Plan {
    let unit = install_files(o.system)
        .into_iter()
        .next()
        .unwrap_or_default();
    let env = env_path(o.system);
    let (wanted, state) = if o.system {
        (
            "multi-user.target",
            "DynamicUser=yes\nStateDirectory=sysview\nEnvironment=SYSVIEW_STATE=/var/lib/sysview\n",
        )
    } else {
        ("default.target", "")
    };

    // Hardening copied from scripts/setup-debian.sh, minus the flags that
    // would break the sampler itself (see the note there).
    let content = format!(
        "[Unit]\n\
         Description=sysview system dashboard\n\
         Documentation={REPO}\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         EnvironmentFile={env}\n\
         {state}\
         ExecStart={exec}\n\
         Restart=always\n\
         RestartSec=3\n\
         NoNewPrivileges=true\n\
         ProtectSystem=strict\n\
         ProtectHome=true\n\
         PrivateTmp=true\n\
         ProtectKernelTunables=true\n\
         ProtectKernelModules=true\n\
         ProtectKernelLogs=true\n\
         ProtectControlGroups=true\n\
         ProtectHostname=true\n\
         RestrictSUIDSGID=true\n\
         LockPersonality=true\n\
         RestrictNamespaces=true\n\
         RestrictRealtime=true\n\
         \n\
         [Install]\n\
         WantedBy={wanted}\n",
        env = env.display(),
        exec = command_line(&o.argv()),
    );

    let user = !o.system;
    let mut cmds = vec![systemctl(user, "daemon-reload")];
    let mut enable = systemctl(user, "enable");
    enable.push(NAME.to_string());
    cmds.push(enable);
    // `enable --now` also starts; without --now, start explicitly so the
    // summary is never "enabled but not running".
    let mut start = systemctl(user, "start");
    start.push(NAME.to_string());
    cmds.push(start);

    Plan {
        supervisor: "systemd",
        file: Some(install_files(o.system).remove(0)),
        content,
        secret: Some((env, format!("SYSVIEW_TOKEN={}\n", o.token))),
        commands: cmds,
    }
}

#[cfg(target_os = "linux")]
fn systemctl(user: bool, verb: &str) -> Vec<String> {
    let mut v = vec!["systemctl".to_string()];
    if user {
        v.push("--user".to_string());
    }
    v.push(verb.to_string());
    v
}

#[cfg(target_os = "linux")]
fn uninstall_commands(system: bool) -> Vec<Vec<String>> {
    let user = !system;
    let mut disable = systemctl(user, "disable");
    disable.extend(["--now".to_string(), NAME.to_string()]);
    vec![disable, systemctl(user, "daemon-reload")]
}

#[cfg(target_os = "linux")]
fn status_commands(system: bool) -> Vec<Vec<String>> {
    let mut v = systemctl(!system, "status");
    v.extend(["--no-pager".to_string(), NAME.to_string()]);
    vec![v]
}

#[cfg(target_os = "linux")]
fn install_files(system: bool) -> Vec<PathBuf> {
    let dir = if system {
        PathBuf::from("/etc/systemd/system")
    } else {
        PathBuf::from(format!(
            "{}/.config/systemd/user",
            std::env::var("HOME").unwrap_or_default()
        ))
    };
    vec![dir.join(format!("{NAME}.service"))]
}

#[cfg(target_os = "linux")]
fn env_path(system: bool) -> PathBuf {
    if system {
        PathBuf::from("/etc/sysview/env")
    } else {
        PathBuf::from(format!(
            "{}/.config/sysview/env",
            std::env::var("HOME").unwrap_or_default()
        ))
    }
}

#[cfg(target_os = "linux")]
fn manage_hint(system: bool) -> String {
    if system {
        format!("systemctl status {NAME}")
    } else {
        format!("systemctl --user status {NAME}")
    }
}

// --------------------------------------------------------------- macOS -----

#[cfg(target_os = "macos")]
const LABEL: &str = "dev.sysview.dashboard";

#[cfg(target_os = "macos")]
fn platform_plan(o: &Options) -> Plan {
    let plist = install_files(o.system).remove(0);
    let mut args = String::new();
    for a in o.argv() {
        args.push_str(&format!("    <string>{}</string>\n", xml(&a)));
    }
    let content = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20 <key>Label</key>\n\
         \x20 <string>{LABEL}</string>\n\
         \x20 <key>ProgramArguments</key>\n\
         \x20 <array>\n\
         {args}\
         \x20 </array>\n\
         \x20 <key>EnvironmentVariables</key>\n\
         \x20 <dict>\n\
         \x20   <key>SYSVIEW_TOKEN</key>\n\
         \x20   <string>{token}</string>\n\
         \x20 </dict>\n\
         \x20 <key>RunAtLoad</key>\n\
         \x20 <true/>\n\
         \x20 <key>KeepAlive</key>\n\
         \x20 <true/>\n\
         \x20 <key>StandardOutPath</key>\n\
         \x20 <string>/tmp/sysview.log</string>\n\
         \x20 <key>StandardErrorPath</key>\n\
         \x20 <string>/tmp/sysview.log</string>\n\
         </dict>\n\
         </plist>\n",
        token = xml(&o.token),
    );
    let path = plist.display().to_string();
    Plan {
        supervisor: "launchd",
        file: Some(plist),
        content,
        secret: None,
        commands: vec![
            vec!["launchctl".to_string(), "unload".to_string(), path.clone()],
            vec![
                "launchctl".to_string(),
                "load".to_string(),
                "-w".to_string(),
                path,
            ],
        ],
    }
}

#[cfg(target_os = "macos")]
fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(target_os = "macos")]
fn uninstall_commands(system: bool) -> Vec<Vec<String>> {
    let path = install_files(system).remove(0).display().to_string();
    vec![vec![
        "launchctl".to_string(),
        "unload".to_string(),
        "-w".to_string(),
        path,
    ]]
}

#[cfg(target_os = "macos")]
fn status_commands(_system: bool) -> Vec<Vec<String>> {
    vec![vec![
        "launchctl".to_string(),
        "list".to_string(),
        LABEL.to_string(),
    ]]
}

#[cfg(target_os = "macos")]
fn install_files(system: bool) -> Vec<PathBuf> {
    if system {
        vec![PathBuf::from(format!(
            "/Library/LaunchDaemons/{LABEL}.plist"
        ))]
    } else {
        vec![PathBuf::from(format!(
            "{}/Library/LaunchAgents/{LABEL}.plist",
            std::env::var("HOME").unwrap_or_default()
        ))]
    }
}

#[cfg(target_os = "macos")]
fn manage_hint(_system: bool) -> String {
    format!("launchctl list {LABEL}")
}

// ------------------------------------------------------------- windows -----

#[cfg(windows)]
fn platform_plan(o: &Options) -> Plan {
    // ONSTART needs an elevated shell; ONLOGON does not, and is the default.
    let (schedule, when) = if o.system {
        ("ONSTART", "startup")
    } else {
        ("ONLOGON", "logon")
    };
    let mut argv = o.argv();
    argv.push("--token".to_string());
    argv.push(o.token.clone());
    let task = command_line(&argv);

    let mut cmd = vec![
        "schtasks".to_string(),
        "/Create".to_string(),
        "/F".to_string(),
        "/SC".to_string(),
        schedule.to_string(),
    ];
    if o.system {
        // Without a principal an ONSTART task only fires at an interactive
        // logon, which is not what "machine-wide" means.
        cmd.extend([
            "/RU".to_string(),
            "SYSTEM".to_string(),
            "/RL".to_string(),
            "HIGHEST".to_string(),
        ]);
    }
    cmd.extend([
        "/TN".to_string(),
        NAME.to_string(),
        "/TR".to_string(),
        task.clone(),
    ]);

    Plan {
        supervisor: "Task Scheduler",
        file: None,
        content: format!("{task}\n\n(registered to run at {when})\n"),
        secret: None,
        commands: vec![cmd],
    }
}

#[cfg(windows)]
fn uninstall_commands(_system: bool) -> Vec<Vec<String>> {
    vec![vec![
        "schtasks".to_string(),
        "/Delete".to_string(),
        "/F".to_string(),
        "/TN".to_string(),
        NAME.to_string(),
    ]]
}

#[cfg(windows)]
fn status_commands(_system: bool) -> Vec<Vec<String>> {
    vec![vec![
        "schtasks".to_string(),
        "/Query".to_string(),
        "/TN".to_string(),
        NAME.to_string(),
    ]]
}

#[cfg(windows)]
fn install_files(_system: bool) -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(windows)]
fn manage_hint(_system: bool) -> String {
    format!("schtasks /Query /TN {NAME} /V /FO LIST")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_quoted_only_where_a_parser_needs_it() {
        assert_eq!(quote("sysview"), "sysview");
        assert_eq!(quote("--port"), "--port");
        assert_eq!(quote("/usr/local/bin/sysview"), "/usr/local/bin/sysview");
        assert_eq!(
            quote("C:\\Program Files\\sysview.exe"),
            "\"C:\\Program Files\\sysview.exe\""
        );
    }

    #[test]
    fn a_command_line_round_trips_the_flags() {
        let argv: Vec<String> = ["/usr/local/bin/sysview", "serve", "--port", "8080"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            command_line(&argv),
            "/usr/local/bin/sysview serve --port 8080"
        );
    }

    #[test]
    fn the_wildcard_bind_becomes_a_usable_url() {
        assert_eq!(host_for_url("0.0.0.0", 8080), "localhost:8080");
        assert_eq!(host_for_url("127.0.0.1", 80), "127.0.0.1:80");
    }
}
