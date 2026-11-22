//! `wsx web` and `wsx web open [<site>]`: open the web client of a site that is serving now.
//! ref: docs/web-client.md "Open"

use std::io::{BufRead, IsTerminal, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::site::{self, Site};

const PROBE: Duration = Duration::from_millis(800);

/// A site whose server answers on its loopback port.
struct Serving {
    site: Site,
    url: String,
}

/// Open the site that serves, asking which when several do. `pair` adds a one-time pairing
/// code to the URL fragment, so an unpaired browser pairs with one tap. `print` prints the URL
/// without opening a browser.
pub fn open(named: Option<&str>, pair: bool, print: bool) -> Result<()> {
    let files = match named {
        Some(_) => vec![Site::resolve(named)?],
        None => site::files()?,
    };
    if files.is_empty() {
        // ^ The same text as every other `wsx web` command with no site file.
        Site::resolve(None)?;
    }
    let mut serving = Vec::new();
    let mut idle = Vec::new();
    for file in &files {
        let site = match Site::load(file) {
            Ok(site) => site,
            // ^ A named site's error is the answer; an unnamed one only drops out of the list.
            Err(error) if named.is_some() => return Err(error),
            Err(error) => {
                eprintln!("wsx web: skipped {}: {error:#}", file.display());
                continue;
            }
        };
        if serves(&site) {
            let url = format!("{}{}", site.public.origin, site.scope());
            serving.push(Serving { site, url });
        } else {
            idle.push(site.id);
        }
    }
    let chosen = match serving.len() {
        0 if named.is_some() => bail!(
            "site '{0}' is not serving: start it with `wsx web serve {0}`",
            idle.join(", ")
        ),
        0 => bail!(
            "no site is serving{}: start one with `wsx web serve [<site>]`",
            if idle.is_empty() {
                String::new()
            } else {
                format!(" (sites: {})", idle.join(", "))
            }
        ),
        1 => serving.remove(0),
        _ => {
            let at = pick(&serving)?;
            serving.swap_remove(at)
        }
    };
    let mut url = chosen.url;
    if pair {
        let code = crate::devices::Devices::open(&chosen.site.state_dir)?.create_pairing()?;
        // ^ The fragment never reaches a server or a proxy log (docs/web-client.md "Security").
        url = format!("{url}#pair={code}");
    }
    println!("{url}");
    if !print {
        browse(&url)?;
    }
    Ok(())
}

/// Whether the site's own server answers on its port. The server refuses every `Host` but its
/// public one, so a 200 for that host is this site, not another one on the same port.
fn serves(site: &Site) -> bool {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, site.port));
    let Ok(mut stream) = TcpStream::connect_timeout(&address, PROBE) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(PROBE));
    let _ = stream.set_write_timeout(Some(PROBE));
    let request = format!(
        "GET {}manifest.webmanifest HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        site.scope(),
        site.public.host
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut head = [0u8; 12];
    stream.read_exact(&mut head).is_ok() && head.starts_with(b"HTTP/1.1 200")
}

/// The owner's choice among the serving sites: a number on a terminal, else an error that
/// lists them, since a script cannot answer.
fn pick(serving: &[Serving]) -> Result<usize> {
    let list: String = serving
        .iter()
        .enumerate()
        .map(|(at, one)| format!("\n  {}  {:<16} {}", at + 1, one.site.id, one.url))
        .collect();
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        bail!(
            "{} sites are serving; name one: wsx web open <site>{list}",
            serving.len()
        );
    }
    eprint!(
        "{} sites are serving:{list}\nopen which? [1-{}] ",
        serving.len(),
        serving.len()
    );
    let mut answer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .context("cannot read the choice")?;
    match answer.trim().parse::<usize>() {
        Ok(number) if (1..=serving.len()).contains(&number) => Ok(number - 1),
        _ => bail!("no site chosen"),
    }
}

/// The system's URL opener. `WSX_BROWSER` replaces it (a scenario records the URL with it).
fn browse(url: &str) -> Result<()> {
    let opener = std::env::var("WSX_BROWSER").unwrap_or_else(|_| {
        if cfg!(target_os = "macos") {
            "open".into()
        } else {
            "xdg-open".into()
        }
    });
    let status = std::process::Command::new(&opener)
        .arg(url)
        .status()
        .with_context(|| format!("cannot run {opener}"))?;
    if !status.success() {
        bail!("{opener} failed ({status}); open the URL above by hand");
    }
    Ok(())
}
