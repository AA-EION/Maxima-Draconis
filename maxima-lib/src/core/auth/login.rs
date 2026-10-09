use std::time::Duration;

use lazy_static::lazy_static;
use regex::Regex;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use crate::core::{auth::storage::AuthError, clients::JUNO_PC_CLIENT_ID};

use super::context::AuthContext;

lazy_static! {
    static ref HTTP_PATTERN: Regex =
        Regex::new(r"^([A-Za-z]+) +(.*) +(HTTP/[0-9][.][0-9])").unwrap();
}

/// Port EA's `qrc://` login redirect is forwarded to on the host loopback.
pub const LOGIN_CALLBACK_PORT: u16 = 31033;
const LOGIN_WAIT: Duration = Duration::from_secs(300);

/// Bind the login callback. EA redirects every login to the same port, and
/// the host and every Wine prefix share one loopback, so only one Maxima can
/// log in at a time: wait for another login to finish rather than fail.
async fn bind_login_callback() -> Result<TcpListener, AuthError> {
    let deadline = tokio::time::Instant::now() + LOGIN_WAIT;
    let mut announced = false;
    loop {
        match TcpListener::bind(("127.0.0.1", LOGIN_CALLBACK_PORT)).await {
            Ok(listener) => return Ok(listener),
            Err(err)
                if err.kind() == std::io::ErrorKind::AddrInUse
                    && tokio::time::Instant::now() < deadline =>
            {
                if !announced {
                    log::info!("Another Maxima login is in progress; waiting for it to finish...");
                    announced = true;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(err) => return Err(err.into()),
        }
    }
}

pub async fn begin_oauth_login_flow<'a>(context: &mut AuthContext<'a>) -> Result<(), AuthError> {
    let auth_url = context.nucleus_auth_url(JUNO_PC_CLIENT_ID, "code")?;
    // Bind before opening the browser so a fast redirect can't beat us.
    let listener = bind_login_callback().await?;
    // Not fatal: a detached server (or a headless box) may have no browser
    // handler, and the user can still open the logged URL by hand.
    if let Err(err) = open::that(&auth_url) {
        log::warn!("Couldn't open a browser ({}). Open this URL to log in: {}", err, auth_url);
    }

    println!("============================================================");
    println!("Please log in via the browser window that just opened.");
    println!("");
    println!("MAC / CROSSOVER USERS:");
    println!("If your browser gets stuck on signin.ea.com or blocks the qrc:// link:");
    println!("1. Right-click the EA page and select 'Inspect Element'");
    println!("2. Go to Storage or Application tab -> Cookies -> ea.com");
    println!("3. Copy the value of the 'remid' cookie and paste it below:");
    println!("============================================================");

    let mut stdin_reader = tokio::io::BufReader::new(tokio::io::stdin());
    let mut stdin_line = String::new();
    // A detached server (or any process spawned with a null stdin) reads EOF
    // immediately and forever; without this guard the select! spins at 100%
    // CPU for the whole time the user spends in the browser.
    let mut stdin_open = true;

    loop {
        tokio::select! {
            accept_res = listener.accept() => {
                let (mut socket, _) = accept_res?;
                let (read, _) = socket.split();
                let mut reader = BufReader::new(read);

                let mut line = String::new();
                reader.read_line(&mut line).await?;

                let captures = match HTTP_PATTERN.captures(&line) {
                    Some(cap) => cap,
                    None => continue,
                };

                let path_and_query = captures.get(2).ok_or(AuthError::Query)?.as_str();
                if path_and_query.starts_with("/auth") {
                    let query = path_and_query
                        .split_once("?")
                        .map(|(_, qs)| qs.trim())
                        .map(querystring::querify)
                        .ok_or(AuthError::Query)?;

                    // EA may or may not echo `state`; when it does, it must be
                    // ours. PKCE still binds the code to this login either way.
                    let state = query.iter().find(|(k, _)| *k == "state").map(|(_, v)| *v);
                    if state.is_some_and(|s| s != context.state()) {
                        log::warn!("Ignoring a login redirect meant for another Maxima instance");
                        let _ = socket
                            .write_all(b"HTTP/1.1 409 Conflict\r\nContent-Length: 0\r\n\r\n")
                            .await;
                        continue;
                    }

                    let code = query.iter().find(|(k, _)| *k == "code").map(|(_, v)| *v);
                    let _ = socket
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    match code {
                        Some(code) => {
                            context.set_code(code);
                            return Ok(());
                        }
                        None => return Err(AuthError::NoAuthCode.into()),
                    }
                }
            }
            read_res = stdin_reader.read_line(&mut stdin_line), if stdin_open => {
                if read_res? == 0 {
                    stdin_open = false;
                    continue;
                }
                let line = stdin_line.trim();

                // Try parsing as URL first
                let mut found_code = false;
                if let Some((_, qs)) = line.split_once("?") {
                    let query = querystring::querify(qs);
                    for query in query {
                        if query.0 == "code" {
                            context.set_code(query.1);
                            found_code = true;
                            break;
                        }
                    }
                }

                if found_code {
                    return Ok(());
                }

                // If not a URL, it might be a remid cookie
                if line.len() > 30 && !line.starts_with("http") {
                    println!("Attempting to use input as 'remid' cookie...");
                    let client = reqwest::Client::builder()
                        .redirect(reqwest::redirect::Policy::none())
                        .build()
                        .unwrap();

                    let url = context.nucleus_auth_url(JUNO_PC_CLIENT_ID, "code")?;
                    // Clean up the cookie string if the user pasted "remid=..."
                    let cookie_val = if line.starts_with("remid=") { &line[6..] } else { line };
                    let cookie_header = format!("remid={}", cookie_val);

                    if let Ok(res) = client.get(&url).header("Cookie", cookie_header).send().await {
                        if res.status().is_redirection() {
                            if let Some(location) = res.headers().get("location") {
                                if let Ok(loc_str) = location.to_str() {
                                    if let Some((_, qs)) = loc_str.split_once("?") {
                                        let query = querystring::querify(qs);
                                        for q in query {
                                            if q.0 == "code" {
                                                context.set_code(q.1);
                                                println!("Successfully authenticated via cookie!");
                                                return Ok(());
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                if !line.is_empty() {
                    println!("Invalid URL or cookie. Please try again.");
                }
                stdin_line.clear();
            }
        }
    }
}

// Use the OOA API to retrieve an access token without a captcha
#[deprecated(note = "This method of login was patched and this function will be removed soon")]
pub async fn manual_login(_persona: &str, _password: &str) -> Result<String, AuthError> {
    unimplemented!();
}
