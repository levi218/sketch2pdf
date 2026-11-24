use std::{path::PathBuf, process::ExitCode};

const USAGE: &str = "usage: sketch2pdf <input.sketch> [-o] [output.pdf] [--fonts DIR]...";

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // --fonts DIR (repeatable): every .ttf/.otf/.ttc in DIR, recursively.
    let mut fonts = Vec::new();
    while let Some(i) = args.iter().position(|a| a == "--fonts") {
        let Some(dir) = args.get(i + 1).cloned() else {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        };
        args.drain(i..i + 2);
        read_fonts(&PathBuf::from(dir), &mut fonts);
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (input, output) = match args.as_slice() {
        [i] => (PathBuf::from(i), PathBuf::from(i).with_extension("pdf")),
        [i, o] | [i, "-o", o] => (PathBuf::from(i), PathBuf::from(o)),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let result = std::fs::read(&input)
        .map_err(|e| e.to_string())
        .and_then(|bytes| sketch2pdf::convert_with_fonts(&bytes, &fonts).map_err(|e| e.to_string()))
        .and_then(|out| {
            for w in &out.warnings {
                eprintln!("warning: {w}");
            }
            std::fs::write(&output, out.pdf).map_err(|e| e.to_string())
        });
    match result {
        Ok(()) => {
            println!("{}", output.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{}: {e}", input.display());
            ExitCode::FAILURE
        }
    }
}

fn read_fonts(dir: &PathBuf, out: &mut Vec<Vec<u8>>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        eprintln!("warning: cannot read font directory {}", dir.display());
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            read_fonts(&p, out);
        } else if p.extension().is_some_and(|x| ["ttf", "otf", "ttc"].contains(&x.to_ascii_lowercase().to_str().unwrap_or(""))) {
            out.extend(std::fs::read(&p).ok());
        }
    }
}
