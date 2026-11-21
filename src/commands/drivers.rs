//! `tmac driver list|show|check` (§3.2): the driver files.

use std::path::Path;

use crate::driver::{Driver, Source};
use crate::error::{Error, Result};
use crate::home::Home;

fn source_text(d: &Driver) -> String {
    match d.source() {
        Source::Builtin => "built-in".into(),
        Source::File(p, true) => format!("{} (replaces the built-in one)", p.display()),
        Source::File(p, false) => p.display().to_string(),
    }
}

/// Every driver and where it comes from.
pub fn list() -> Result<()> {
    let home = Home::resolve()?;
    for name in Driver::names(&home)? {
        match Driver::load(&home, &name) {
            Ok(d) => println!("{name}\t{}", source_text(&d)),
            Err(e) => println!("{name}\tinvalid: {e}"),
        }
    }
    Ok(())
}

/// The file of a driver, as written (to copy and change).
pub fn show(name: &str) -> Result<()> {
    let home = Home::resolve()?;
    print!("{}", Driver::load(&home, name)?.text());
    Ok(())
}

/// Check a driver given by name or file, or every driver.
pub fn check(target: Option<&str>) -> Result<()> {
    let home = Home::resolve()?;
    let results: Vec<(String, Result<Driver>)> = match target {
        Some(t) if t.contains('/') || t.ends_with(".yaml") => {
            vec![(t.to_string(), Driver::parse_file(Path::new(t)))]
        }
        Some(name) => vec![(name.to_string(), Driver::load(&home, name))],
        None => Driver::names(&home)?
            .into_iter()
            .map(|n| {
                let r = Driver::load(&home, &n);
                (n, r)
            })
            .collect(),
    };
    let mut bad = 0;
    for (what, r) in results {
        match r {
            Ok(d) => println!(
                "{what}: ok ({}, {} events)",
                source_text(&d),
                d.events().len()
            ),
            Err(e) => {
                bad += 1;
                println!("{what}: {e}");
            }
        }
    }
    if bad > 0 {
        return Err(Error::usage(format!("{bad} invalid driver file(s)")));
    }
    Ok(())
}
