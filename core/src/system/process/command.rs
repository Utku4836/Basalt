//! Batch escaping adapted from node-cross-spawn (MIT); see vendor/cmd-escaping.

use crate::{Error, ErrorKind, Result, profiles::environment::Environment};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

pub struct Invocation {
    pub program: PathBuf,
    pub arguments: Vec<String>,
    pub verbatim: bool,
}

pub fn resolve(
    program: &Path,
    arguments: &[String],
    environment: &Environment,
) -> Result<Invocation> {
    let batch = program
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
        });
    if !batch {
        return Ok(Invocation {
            program: program.to_owned(),
            arguments: arguments.to_vec(),
            verbatim: false,
        });
    }
    let get = |name: &str| {
        environment
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    };
    let shell = get("COMSPEC")
        .map(PathBuf::from)
        .or_else(|| {
            get("SYSTEMROOT").map(|base| PathBuf::from(base).join("System32").join("cmd.exe"))
        })
        .ok_or_else(|| {
            Error::new(
                ErrorKind::NotFound,
                "A Windows command interpreter is required for this batch command",
            )
        })?;
    if !shell.is_absolute() || !shell.is_file() {
        return Err(Error::new(
            ErrorKind::NotFound,
            "Windows command interpreter was not found",
        ));
    }
    let path = program
        .to_str()
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "Batch paths must be Unicode"))?;
    if path.contains(['\0', '\r', '\n'])
        || arguments
            .iter()
            .any(|argument| argument.contains(['\0', '\r', '\n']))
    {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Batch command contains invalid control characters",
        ));
    }
    let size = fs::metadata(program)
        .map_err(|error| Error::io("Inspect batch wrapper", program, error))?
        .len();
    if size > 65536 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Batch wrapper is too large to inspect",
        ));
    }
    let mut script = Vec::new();
    fs::File::open(program)
        .map_err(|error| Error::io("Open batch wrapper", program, error))?
        .take(65537)
        .read_to_end(&mut script)
        .map_err(|error| Error::io("Read batch wrapper", program, error))?;
    if script.len() > 65536 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Batch wrapper grew beyond its inspection limit",
        ));
    }
    let forwarding = String::from_utf8_lossy(&script).contains("%*");
    let mut command = escape_meta(&path.replace('/', "\\"));
    for argument in arguments {
        command.push(' ');
        command.push_str(&escape_argument(argument, forwarding));
    }
    Ok(Invocation {
        program: shell,
        arguments: vec![
            "/d".into(),
            "/v:off".into(),
            "/s".into(),
            "/c".into(),
            format!("\"{command}\""),
        ],
        verbatim: true,
    })
}

fn escape_meta(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if "()[]%!^\"`<>&|;, *?".contains(character) {
            escaped.push('^');
        }
        escaped.push(character);
    }
    escaped
}
fn escape_argument(value: &str, double: bool) -> String {
    let mut quoted = String::from("\"");
    let mut slashes = 0;
    for character in value.chars() {
        if character == '\\' {
            slashes += 1;
            continue;
        }
        if character == '\"' {
            quoted.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
        } else {
            quoted.extend(std::iter::repeat_n('\\', slashes));
        }
        slashes = 0;
        quoted.push(character);
    }
    quoted.extend(std::iter::repeat_n('\\', slashes * 2));
    quoted.push('\"');
    let escaped = escape_meta(&quoted);
    if double {
        escape_meta(&escaped)
    } else {
        escaped
    }
}
