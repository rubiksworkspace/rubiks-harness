// Copied from Rubiks IDE (a Zed fork), crates/askpass/src/askpass.rs, at commit 908f62b01c.
// Only the items the Zed CLI uses are kept; see vendor/zed-cli/README.md.

/// Runs Zed in netcat mode for use in askpass.
pub fn main(socket: &str) {
    use std::io::{self, Read};
    use std::process::exit;

    let mut buffer = Vec::new();
    if let Err(err) = io::stdin().read_to_end(&mut buffer) {
        eprintln!("Error reading from stdin: {}", err);
        exit(1);
    }

    connect_and_write_prompt(socket, buffer)
}

/// Runs Zed in askpass mode using prompts passed as arguments.
pub fn main_from_args(socket: &str, args: impl IntoIterator<Item = String>) {
    let prompt = args.into_iter().collect::<Vec<_>>().join("\0");
    connect_and_write_prompt(socket, prompt.into_bytes())
}

fn connect_and_write_prompt(socket: &str, mut buffer: Vec<u8>) {
    use net::UnixStream;
    use std::io::{self, Read, Write};
    use std::process::exit;

    let mut stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!("Error connecting to socket {}: {}", socket, err);
            exit(1);
        }
    };

    #[cfg(target_os = "windows")]
    while buffer.last().is_some_and(|&b| b == b'\n' || b == b'\r') {
        buffer.pop();
    }
    if buffer.last() != Some(&b'\0') {
        buffer.push(b'\0');
    }

    if let Err(err) = stream.write_all(&buffer) {
        eprintln!("Error writing to socket: {}", err);
        exit(1);
    }

    let mut response = Vec::new();
    if let Err(err) = stream.read_to_end(&mut response) {
        eprintln!("Error reading from socket: {}", err);
        exit(1);
    }

    if let Err(err) = io::stdout().write_all(&response) {
        eprintln!("Error writing to stdout: {}", err);
        exit(1);
    }
}
