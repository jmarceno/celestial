//! Browser login for Google Drive, Dropbox, and pCloud.
//!
//! Google Drive still uses the local page on port 8000 so the user can open
//! rclone's auth URL from a normal browser. Dropbox and pCloud let rclone
//! open the browser itself.

use nix::{
    sys::signal::{self, Signal},
    unistd::Pid,
};
use rocket::response::{Responder, content::RawHtml};
use std::{
    io::{BufRead, BufReader},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use tera::{Context, Tera};

static STATE_URL: Mutex<String> = Mutex::new(String::new());

#[derive(Clone, Copy, Debug)]
pub enum AuthType {
    Dropbox,
    GDrive,
    PCloud,
}

impl std::fmt::Display for AuthType {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(match self {
            Self::Dropbox => "Dropbox",
            Self::GDrive => "Google Drive",
            Self::PCloud => "pCloud",
        })
    }
}

pub fn default_client(kind: AuthType) -> (&'static str, &'static str) {
    match kind {
        AuthType::Dropbox => ("hke0fgr43viaq03", "o4cpx8trcnneq7a"),
        AuthType::GDrive => (
            "617798216802-gpgajsc7o768ukbdegk5esa3jf6aekgj.apps.googleusercontent.com",
            "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",
        ),
        AuthType::PCloud => ("KRzpo46NKb7", "g10qvqgWR85lSvEQWlIqCmPYIhwX"),
    }
}

#[rocket::get("/")]
fn get_google_drive() -> RawHtml<String> {
    let mut context = Context::new();
    context.insert("state_url", STATE_URL.lock().unwrap().as_str());
    RawHtml(
        Tera::one_off(
            include_str!("../html/google-drive.tera.html"),
            &context,
            true,
        )
        .unwrap(),
    )
}

#[derive(Responder)]
#[response(content_type = "image/png")]
struct PngResponse(&'static [u8]);

#[rocket::get("/google-signin.png")]
fn get_google_signin_png() -> PngResponse {
    PngResponse(include_bytes!("../images/google-signin.png"))
}

#[rocket::get("/google-drive.png")]
fn get_google_drive_png() -> PngResponse {
    PngResponse(include_bytes!("../images/google-drive.png"))
}

/// Run `rclone authorize` and return the token JSON line.
///
/// `cancel` is set by the UI. `on_waiting` runs once the browser step has
/// started, so the window can show Cancel.
pub fn authorize(
    kind: AuthType,
    cancel: &AtomicBool,
    on_waiting: impl FnOnce(),
) -> Result<String, String> {
    let (client_id, client_secret) = default_client(kind);
    let provider = match kind {
        AuthType::GDrive => "drive",
        AuthType::Dropbox => "dropbox",
        AuthType::PCloud => "pcloud",
    };
    let mut args = vec!["authorize", provider, client_id, client_secret];
    if matches!(kind, AuthType::GDrive) {
        args.push("--auth-no-open-browser");
    }

    let mut process = Command::new("rclone")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("Could not start rclone: {err}"))?;

    let process_stdout = Arc::new(Mutex::new(String::new()));
    let process_stderr = Arc::new(Mutex::new(String::new()));
    let stdout_handle = process.stdout.take().unwrap();
    let stderr_handle = process.stderr.take().unwrap();
    let stdout_buf = process_stdout.clone();
    thread::spawn(move || {
        let reader = BufReader::new(stdout_handle);
        for line in reader.lines().map_while(Result::ok) {
            let mut buf = stdout_buf.lock().unwrap();
            buf.push_str(&line);
            buf.push('\n');
        }
    });
    let stderr_buf = process_stderr.clone();
    thread::spawn(move || {
        let reader = BufReader::new(stderr_handle);
        for line in reader.lines().map_while(Result::ok) {
            let mut buf = stderr_buf.lock().unwrap();
            buf.push_str(&line);
            buf.push('\n');
        }
    });

    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = signal::kill(
                Pid::from_raw(process.id().try_into().unwrap_or(0)),
                Signal::SIGTERM,
            );
            return Err("cancelled".into());
        }
        if process.try_wait().map_err(|e| e.to_string())?.is_some() {
            break;
        }
        let process_output = format!(
            "{}\n{}",
            process_stdout.lock().unwrap(),
            process_stderr.lock().unwrap(),
        );
        if let Some(line) = process_output
            .lines()
            .find(|line| line.contains("http://127.0.0.1:53682/auth"))
        {
            *STATE_URL.lock().unwrap() = line.split_whitespace().last().unwrap_or("").to_owned();
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }

    tracing::info!("oauth state url ready for {kind}");
    let runtime = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    let handle = runtime.spawn(
        rocket::build()
            .mount(
                "/",
                rocket::routes![
                    get_google_drive,
                    get_google_signin_png,
                    get_google_drive_png
                ],
            )
            .launch(),
    );
    if matches!(kind, AuthType::GDrive) {
        let status = Command::new("xdg-open")
            .arg("http://localhost:8000")
            .spawn()
            .map_err(|err| format!("Could not open a browser: {err}"))?
            .wait()
            .map_err(|err| format!("Could not open a browser: {err}"))?;
        if !status.success() {
            return Err(format!("xdg-open exited with {status}"));
        }
    }
    on_waiting();

    loop {
        thread::sleep(Duration::from_millis(200));
        if cancel.load(Ordering::Relaxed) {
            handle.abort();
            let _ = signal::kill(
                Pid::from_raw(process.id().try_into().unwrap_or(0)),
                Signal::SIGTERM,
            );
            return Err("cancelled".into());
        }
        if handle.is_finished() {
            let error_string = match crate::util::await_future(handle) {
                Ok(Err(err)) => err.to_string(),
                Ok(Ok(_rocket)) => "webserver stopped".to_string(),
                Err(err) => err.to_string(),
            };
            return Err(error_string);
        }
        if let Some(exit_status) = process.try_wait().map_err(|e| e.to_string())? {
            handle.abort();
            if !exit_status.success() {
                return Err(process_stderr.lock().unwrap().clone());
            }
            let stdout = process_stdout.lock().unwrap().clone();
            let lines: Vec<&str> = stdout.lines().collect();
            let token = lines
                .len()
                .checked_sub(2)
                .and_then(|i| lines.get(i))
                .map(|s| (*s).to_owned())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    format!(
                        "rclone did not return a token.\n{}",
                        process_stderr.lock().unwrap()
                    )
                })?;
            return Ok(token);
        }
    }
}
