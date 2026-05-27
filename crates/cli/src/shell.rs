//! Interactive database shell.
//!
//! Provides a REPL for database operations with support for:
//! - Auto-commit mode: single commands wrap in begin/commit
//! - Explicit transaction mode: named transactions for SSI demos

use kv::{Db, Options};
use runtime::{Env, Path, RealEnv};
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Run the shell with stdin/stdout using RealEnv.
pub fn run_shell_main(path: &std::path::Path) -> Result<(), String> {
    // Set up Ctrl-C handler
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .map_err(|e| format!("failed to set ctrl-c handler: {}", e))?;

    let env = RealEnv::new();
    let path_ref = Path::new(path.to_str().ok_or("invalid path")?);

    let db = Db::open(env, path_ref, Options::default()).map_err(|e| {
        if e.to_string().contains("already open") {
            "database already open at this path (only one shell process per path)".to_string()
        } else {
            format!("{}", e)
        }
    })?;

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();

    run_shell(stdin.lock(), stdout.lock(), db, running)
}

/// Run the shell with custom I/O streams.
///
/// This is the core shell loop, factored out for testing.
pub fn run_shell<R, W, E>(
    mut reader: R,
    mut writer: W,
    db: Db<E>,
    running: Arc<AtomicBool>,
) -> Result<(), String>
where
    R: BufRead,
    W: Write,
    E: Env + Clone,
{
    let mut state = ShellState::new(db);

    loop {
        // Check if interrupted
        if !running.load(Ordering::SeqCst) {
            writeln!(writer).ok();
            break;
        }

        // Print prompt
        let prompt = state.prompt();
        write!(writer, "{}", prompt).ok();
        writer.flush().ok();

        // Read line
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {
                // EOF
                writeln!(writer).ok();
                break;
            }
            Ok(_) => {}
            Err(e) => {
                writeln!(writer, "error: failed to read input: {}", e).ok();
                continue;
            }
        }

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Parse and execute command
        match parse_command(line) {
            Ok((cmd, args)) => {
                if cmd == "exit" {
                    break;
                }
                match state.execute(&cmd, &args, &mut writer) {
                    Ok(should_exit) => {
                        if should_exit {
                            break;
                        }
                    }
                    Err(e) => {
                        writeln!(writer, "error: {}", e).ok();
                    }
                }
            }
            Err(e) => {
                writeln!(writer, "error: {}", e).ok();
            }
        }
    }

    Ok(())
}

/// Shell state holding the database and active transactions.
struct ShellState<E: Env + Clone> {
    db: Db<E>,
    transactions: std::collections::HashMap<String, kv::Txn<E>>,
    active_txn: Option<String>,
}

impl<E: Env + Clone> ShellState<E> {
    fn new(db: Db<E>) -> Self {
        Self {
            db,
            transactions: std::collections::HashMap::new(),
            active_txn: None,
        }
    }

    fn prompt(&self) -> String {
        match &self.active_txn {
            Some(name) => format!("[txn {}]> ", name),
            None => "> ".to_string(),
        }
    }

    fn execute<W: Write>(
        &mut self,
        cmd: &str,
        args: &[String],
        writer: &mut W,
    ) -> Result<bool, String> {
        match cmd {
            "help" => {
                self.cmd_help(writer);
                Ok(false)
            }
            "put" => {
                self.cmd_put(args, writer)?;
                Ok(false)
            }
            "get" => {
                self.cmd_get(args, writer)?;
                Ok(false)
            }
            "delete" => {
                self.cmd_delete(args, writer)?;
                Ok(false)
            }
            "scan" => {
                self.cmd_scan(args, writer)?;
                Ok(false)
            }
            "info" => {
                self.cmd_info(writer)?;
                Ok(false)
            }
            "compact" => {
                self.cmd_compact(writer)?;
                Ok(false)
            }
            "watermark" => {
                self.cmd_watermark(writer)?;
                Ok(false)
            }
            "begin" => {
                self.cmd_begin(args, writer)?;
                Ok(false)
            }
            "use" => {
                self.cmd_use(args)?;
                Ok(false)
            }
            "list" => {
                self.cmd_list(writer)?;
                Ok(false)
            }
            "commit" => {
                self.cmd_commit(writer)?;
                Ok(false)
            }
            "rollback" => {
                self.cmd_rollback(writer)?;
                Ok(false)
            }
            _ => Err(format!(
                "unknown command: {}. type 'help' for available commands",
                cmd
            )),
        }
    }

    fn cmd_help<W: Write>(&self, writer: &mut W) {
        writeln!(writer, "commands:").ok();
        writeln!(writer, "  put <key> <value>  - store a key-value pair").ok();
        writeln!(writer, "  get <key>          - retrieve a value by key").ok();
        writeln!(writer, "  delete <key>       - delete a key").ok();
        writeln!(writer, "  scan [start] [end] - scan keys in range").ok();
        writeln!(writer, "  info               - show database info").ok();
        writeln!(writer, "  compact            - run compaction").ok();
        writeln!(writer, "  watermark          - show gc watermark").ok();
        writeln!(writer, "  begin <name>       - start a named transaction").ok();
        writeln!(
            writer,
            "  use <name>         - switch to a named transaction"
        )
        .ok();
        writeln!(writer, "  list               - list open transactions").ok();
        writeln!(
            writer,
            "  commit             - commit the active transaction"
        )
        .ok();
        writeln!(
            writer,
            "  rollback           - rollback the active transaction"
        )
        .ok();
        writeln!(writer, "  help               - show this help").ok();
        writeln!(writer, "  exit               - close the shell").ok();
        writeln!(writer).ok();
        writeln!(
            writer,
            "keys and values are byte strings. use 0x prefix for hex:"
        )
        .ok();
        writeln!(
            writer,
            "  put hello world    - stores b\"hello\" = b\"world\""
        )
        .ok();
        writeln!(
            writer,
            "  put 0xdead 0xbeef  - stores bytes [0xde, 0xad] = [0xbe, 0xef]"
        )
        .ok();
    }

    fn cmd_put<W: Write>(&mut self, args: &[String], writer: &mut W) -> Result<(), String> {
        if args.len() != 2 {
            return Err("usage: put <key> <value>".to_string());
        }

        let key = parse_bytes(&args[0])?;
        let value = parse_bytes(&args[1])?;

        if key.is_empty() {
            return Err("key cannot be empty".to_string());
        }
        if value.is_empty() {
            return Err("value cannot be empty".to_string());
        }

        if let Some(txn_name) = &self.active_txn {
            // Operating on active transaction
            let txn = self
                .transactions
                .get_mut(txn_name)
                .ok_or("active transaction not found")?;
            txn.put(&key, &value).map_err(|e| e.to_string())?;
            writeln!(writer, "ok (buffered)").ok();
        } else {
            // Auto-commit mode
            let mut txn = self.db.begin();
            txn.put(&key, &value).map_err(|e| e.to_string())?;
            let outcome = txn.commit().map_err(|e| e.to_string())?;
            if outcome.aborted_for_ssi {
                writeln!(writer, "aborted_for_ssi (retry the operation)").ok();
            } else {
                writeln!(writer, "ok ts={}", outcome.commit_ts).ok();
            }
        }

        Ok(())
    }

    fn cmd_get<W: Write>(&mut self, args: &[String], writer: &mut W) -> Result<(), String> {
        if args.len() != 1 {
            return Err("usage: get <key>".to_string());
        }

        let key = parse_bytes(&args[0])?;
        if key.is_empty() {
            return Err("key cannot be empty".to_string());
        }

        if let Some(txn_name) = &self.active_txn {
            // Operating on active transaction
            let txn = self
                .transactions
                .get_mut(txn_name)
                .ok_or("active transaction not found")?;
            match txn.get(&key).map_err(|e| e.to_string())? {
                Some(value) => {
                    writeln!(writer, "{}", format_value(&value)).ok();
                }
                None => {
                    writeln!(writer, "<not found>").ok();
                }
            }
        } else {
            // Auto-commit mode (read-only, rollback)
            let mut txn = self.db.begin();
            let result = txn.get(&key).map_err(|e| e.to_string())?;
            txn.rollback();
            match result {
                Some(value) => {
                    writeln!(writer, "{}", format_value(&value)).ok();
                }
                None => {
                    writeln!(writer, "<not found>").ok();
                }
            }
        }

        Ok(())
    }

    fn cmd_delete<W: Write>(&mut self, args: &[String], writer: &mut W) -> Result<(), String> {
        if args.len() != 1 {
            return Err("usage: delete <key>".to_string());
        }

        let key = parse_bytes(&args[0])?;
        if key.is_empty() {
            return Err("key cannot be empty".to_string());
        }

        if let Some(txn_name) = &self.active_txn {
            // Operating on active transaction
            let txn = self
                .transactions
                .get_mut(txn_name)
                .ok_or("active transaction not found")?;
            txn.delete(&key).map_err(|e| e.to_string())?;
            writeln!(writer, "ok (buffered)").ok();
        } else {
            // Auto-commit mode
            let mut txn = self.db.begin();
            txn.delete(&key).map_err(|e| e.to_string())?;
            let outcome = txn.commit().map_err(|e| e.to_string())?;
            if outcome.aborted_for_ssi {
                writeln!(writer, "aborted_for_ssi (retry the operation)").ok();
            } else {
                writeln!(writer, "ok ts={}", outcome.commit_ts).ok();
            }
        }

        Ok(())
    }

    fn cmd_scan<W: Write>(&mut self, args: &[String], writer: &mut W) -> Result<(), String> {
        // Parse range bounds
        let (start, end): (std::ops::Bound<Vec<u8>>, std::ops::Bound<Vec<u8>>) = match args.len() {
            0 => (std::ops::Bound::Unbounded, std::ops::Bound::Unbounded),
            1 => {
                let start_key = parse_bytes(&args[0])?;
                (
                    std::ops::Bound::Included(start_key),
                    std::ops::Bound::Unbounded,
                )
            }
            2 => {
                let start_key = parse_bytes(&args[0])?;
                let end_key = parse_bytes(&args[1])?;
                (
                    std::ops::Bound::Included(start_key),
                    std::ops::Bound::Excluded(end_key),
                )
            }
            _ => return Err("usage: scan [start] [end]".to_string()),
        };

        if let Some(txn_name) = &self.active_txn {
            // Operating on active transaction
            let txn = self
                .transactions
                .get_mut(txn_name)
                .ok_or("active transaction not found")?;
            let scan = txn.scan((start, end)).map_err(|e| e.to_string())?;
            for entry in scan {
                let (k, v) = entry.map_err(|e| e.to_string())?;
                writeln!(writer, "{} = {}", format_value(&k), format_value(&v)).ok();
            }
        } else {
            // Auto-commit mode (read-only, rollback)
            let mut txn = self.db.begin();
            let scan = txn.scan((start, end)).map_err(|e| e.to_string())?;
            let results: Vec<_> = scan.collect();
            txn.rollback();
            for entry in results {
                let (k, v) = entry.map_err(|e| e.to_string())?;
                writeln!(writer, "{} = {}", format_value(&k), format_value(&v)).ok();
            }
        }

        Ok(())
    }

    fn cmd_info<W: Write>(&self, writer: &mut W) -> Result<(), String> {
        writeln!(writer, "path: {}", self.db.path().display()).ok();
        writeln!(writer, "active transactions: {}", self.transactions.len()).ok();
        writeln!(writer, "gc watermark: {}", self.db.gc_watermark()).ok();
        Ok(())
    }

    fn cmd_compact<W: Write>(&self, writer: &mut W) -> Result<(), String> {
        self.db.compact_all().map_err(|e| e.to_string())?;
        writeln!(writer, "ok").ok();
        Ok(())
    }

    fn cmd_watermark<W: Write>(&self, writer: &mut W) -> Result<(), String> {
        writeln!(writer, "{}", self.db.gc_watermark()).ok();
        Ok(())
    }

    fn cmd_begin<W: Write>(&mut self, args: &[String], _writer: &mut W) -> Result<(), String> {
        if args.len() != 1 {
            return Err("usage: begin <name>".to_string());
        }

        let name = &args[0];
        if self.transactions.contains_key(name) {
            return Err(format!("transaction '{}' already exists", name));
        }

        let txn = self.db.begin();
        self.transactions.insert(name.clone(), txn);
        self.active_txn = Some(name.clone());
        Ok(())
    }

    fn cmd_use(&mut self, args: &[String]) -> Result<(), String> {
        if args.len() != 1 {
            return Err("usage: use <name>".to_string());
        }

        let name = &args[0];
        if !self.transactions.contains_key(name) {
            return Err(format!("txn '{}' does not exist", name));
        }

        self.active_txn = Some(name.clone());
        Ok(())
    }

    fn cmd_list<W: Write>(&self, writer: &mut W) -> Result<(), String> {
        if self.transactions.is_empty() {
            writeln!(writer, "no open transactions").ok();
        } else {
            let mut names: Vec<_> = self.transactions.keys().collect();
            names.sort();
            for name in names {
                let txn = self.transactions.get(name).unwrap();
                let active_marker = if Some(name) == self.active_txn.as_ref() {
                    " *"
                } else {
                    ""
                };
                writeln!(
                    writer,
                    "  {} (begin_ts={}){}",
                    name,
                    txn.begin_ts(),
                    active_marker
                )
                .ok();
            }
        }
        Ok(())
    }

    fn cmd_commit<W: Write>(&mut self, writer: &mut W) -> Result<(), String> {
        let name = self
            .active_txn
            .take()
            .ok_or("no active transaction to commit")?;

        let txn = self
            .transactions
            .remove(&name)
            .ok_or("active transaction not found")?;

        let outcome = txn.commit().map_err(|e| e.to_string())?;
        if outcome.aborted_for_ssi {
            writeln!(writer, "aborted_for_ssi (retry the operation)").ok();
        } else {
            writeln!(writer, "ok ts={}", outcome.commit_ts).ok();
        }
        Ok(())
    }

    fn cmd_rollback<W: Write>(&mut self, writer: &mut W) -> Result<(), String> {
        let name = self
            .active_txn
            .take()
            .ok_or("no active transaction to rollback")?;

        let txn = self
            .transactions
            .remove(&name)
            .ok_or("active transaction not found")?;

        txn.rollback();
        writeln!(writer, "ok").ok();
        Ok(())
    }
}

/// Parse a command line into command and arguments.
pub fn parse_command(line: &str) -> Result<(String, Vec<String>), String> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.is_empty() {
        return Err("empty command".to_string());
    }

    let cmd = parts[0].to_lowercase();
    let args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();

    Ok((cmd, args))
}

/// Parse a byte string from user input.
///
/// Supports:
/// - Plain ASCII: "hello" -> b"hello"
/// - Hex with 0x prefix: "0xdeadbeef" -> [0xde, 0xad, 0xbe, 0xef]
pub fn parse_bytes(s: &str) -> Result<Vec<u8>, String> {
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        if hex.is_empty() {
            return Err("hex value cannot be empty after 0x prefix".to_string());
        }
        if hex.len() % 2 != 0 {
            return Err("hex value must have even number of digits".to_string());
        }
        let mut bytes = Vec::with_capacity(hex.len() / 2);
        for i in (0..hex.len()).step_by(2) {
            let byte = u8::from_str_radix(&hex[i..i + 2], 16)
                .map_err(|_| format!("invalid hex digit in '{}'", &hex[i..i + 2]))?;
            bytes.push(byte);
        }
        Ok(bytes)
    } else {
        Ok(s.as_bytes().to_vec())
    }
}

/// Format a value for display.
///
/// If the value is valid UTF-8 and printable, display as string.
/// Otherwise, display as hex.
pub fn format_value(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        if s.chars().all(|c| !c.is_control() || c == '\n' || c == '\t') {
            return s.to_string();
        }
    }
    format!("0x{}", hex_encode(bytes))
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(s, "{:02x}", b).unwrap();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_command_simple() {
        let (cmd, args) = parse_command("put hello world").unwrap();
        assert_eq!(cmd, "put");
        assert_eq!(args, vec!["hello", "world"]);
    }

    #[test]
    fn parse_command_single() {
        let (cmd, args) = parse_command("help").unwrap();
        assert_eq!(cmd, "help");
        assert!(args.is_empty());
    }

    #[test]
    fn parse_command_empty() {
        let result = parse_command("");
        assert!(result.is_err());
    }

    #[test]
    fn parse_command_whitespace_only() {
        let result = parse_command("   ");
        assert!(result.is_err());
    }

    #[test]
    fn parse_command_case_insensitive() {
        let (cmd, _) = parse_command("PUT hello world").unwrap();
        assert_eq!(cmd, "put");
    }

    #[test]
    fn parse_bytes_ascii() {
        let bytes = parse_bytes("hello").unwrap();
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn parse_bytes_hex() {
        let bytes = parse_bytes("0xdeadbeef").unwrap();
        assert_eq!(bytes, vec![0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn parse_bytes_hex_uppercase() {
        let bytes = parse_bytes("0XDEADBEEF").unwrap();
        assert_eq!(bytes, vec![0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn parse_bytes_hex_empty() {
        let result = parse_bytes("0x");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("empty"));
    }

    #[test]
    fn parse_bytes_hex_odd_length() {
        let result = parse_bytes("0xabc");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("even"));
    }

    #[test]
    fn parse_bytes_hex_invalid() {
        let result = parse_bytes("0xgg");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid hex"));
    }

    #[test]
    fn format_value_ascii() {
        let s = format_value(b"hello");
        assert_eq!(s, "hello");
    }

    #[test]
    fn format_value_binary() {
        let s = format_value(&[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(s, "0xdeadbeef");
    }

    // Integration tests using SimEnv for determinism

    use kv::{Db, Options};
    use runtime::{Path, SimEnv, SimEnvConfig};
    use std::io::Cursor;

    /// Helper to run shell with given input and return output.
    fn run_shell_with_input(input: &str, seed: u64) -> String {
        let env = SimEnv::new(SimEnvConfig::with_seed(seed));
        // Use unique path per seed to avoid AlreadyOpen errors
        let path_str = format!("/db_{}", seed);
        let path = Path::new(&path_str);
        env.create_dir_all(path).unwrap();

        let db = Db::open(env, path, Options::default()).unwrap();

        let reader = Cursor::new(input.to_string());
        let mut output = Vec::new();
        let running = Arc::new(AtomicBool::new(true));

        run_shell(reader, &mut output, db, running).unwrap();

        String::from_utf8(output).unwrap()
    }

    #[test]
    fn shell_auto_commit_roundtrip() {
        let input = "put hello world\nget hello\nexit\n";
        let output = run_shell_with_input(input, 42);
        assert!(
            output.contains("ok ts="),
            "should contain commit ts: {}",
            output
        );
        assert!(output.contains("world"), "should contain value: {}", output);
    }

    #[test]
    fn shell_get_not_found() {
        let input = "get missing\nexit\n";
        let output = run_shell_with_input(input, 43);
        assert!(
            output.contains("<not found>"),
            "should show not found: {}",
            output
        );
    }

    #[test]
    fn shell_delete_and_verify() {
        let input = "put key value\nget key\ndelete key\nget key\nexit\n";
        let output = run_shell_with_input(input, 44);
        assert!(
            output.contains("value"),
            "should find value initially: {}",
            output
        );
        assert!(
            output.contains("<not found>"),
            "should not find after delete: {}",
            output
        );
    }

    #[test]
    fn shell_hex_key_value() {
        let input = "put 0xdead 0xbeef\nget 0xdead\nexit\n";
        let output = run_shell_with_input(input, 45);
        assert!(output.contains("ok ts="), "should commit: {}", output);
        assert!(
            output.contains("0xbeef"),
            "should show hex value: {}",
            output
        );
    }

    #[test]
    fn shell_empty_key_rejected() {
        let input = "put \"\" value\nexit\n";
        let _output = run_shell_with_input(input, 46);
        // The empty string "" will be parsed as two chars, so this tests something else
        // Let's test with the actual empty scenario
        let input2 = "get 0x\nexit\n";
        let output2 = run_shell_with_input(input2, 47);
        assert!(output2.contains("error:"), "should show error: {}", output2);
    }

    #[test]
    fn shell_begin_and_commit() {
        let input = "begin txn1\nput x 1\ncommit\nget x\nexit\n";
        let output = run_shell_with_input(input, 48);
        assert!(
            output.contains("ok (buffered)"),
            "should buffer write: {}",
            output
        );
        assert!(output.contains("ok ts="), "should commit: {}", output);
        assert!(
            output.contains('1'),
            "should find committed value: {}",
            output
        );
    }

    #[test]
    fn shell_begin_and_rollback() {
        let input = "begin txn1\nput x 1\nrollback\nget x\nexit\n";
        let output = run_shell_with_input(input, 49);
        assert!(
            output.contains("ok (buffered)"),
            "should buffer write: {}",
            output
        );
        assert!(
            output.contains("<not found>"),
            "should not find rolled back value: {}",
            output
        );
    }

    #[test]
    fn shell_list_transactions() {
        let input = "begin a\nbegin b\nlist\nexit\n";
        let output = run_shell_with_input(input, 50);
        assert!(
            output.contains("a (begin_ts="),
            "should list txn a: {}",
            output
        );
        assert!(
            output.contains("b (begin_ts="),
            "should list txn b: {}",
            output
        );
        assert!(output.contains(" *"), "should mark active txn: {}", output);
    }

    #[test]
    fn shell_use_nonexistent_txn() {
        let input = "use nonexistent\nexit\n";
        let output = run_shell_with_input(input, 51);
        assert!(output.contains("error:"), "should show error: {}", output);
        assert!(
            output.contains("does not exist"),
            "should say txn doesn't exist: {}",
            output
        );
    }

    #[test]
    fn shell_begin_duplicate_name() {
        let input = "begin a\nbegin a\nexit\n";
        let output = run_shell_with_input(input, 52);
        assert!(output.contains("error:"), "should show error: {}", output);
        assert!(
            output.contains("already exists"),
            "should say txn already exists: {}",
            output
        );
    }

    #[test]
    fn shell_concurrent_txns_demo_write_write_conflict() {
        let input = "begin a\nput x 1\nbegin b\nput x 2\nuse a\ncommit\nuse b\ncommit\nexit\n";
        let output = run_shell_with_input(input, 53);
        // The second commit should either succeed at higher ts or abort_for_ssi
        // Don't assert on which - just that the shell didn't crash and both txns
        // produced an outcome message.
        let ok_count = output.matches("ok ts=").count();
        let abort_count = output.matches("aborted_for_ssi").count();
        assert!(
            ok_count + abort_count >= 2,
            "both txns should produce outcome: {} oks, {} aborts, output: {}",
            ok_count,
            abort_count,
            output
        );
    }

    #[test]
    fn shell_info_command() {
        let input = "info\nexit\n";
        let output = run_shell_with_input(input, 54);
        assert!(output.contains("path:"), "should show path: {}", output);
        assert!(
            output.contains("active transactions:"),
            "should show txn count: {}",
            output
        );
        assert!(
            output.contains("gc watermark:"),
            "should show watermark: {}",
            output
        );
    }

    #[test]
    fn shell_help_command() {
        let input = "help\nexit\n";
        let output = run_shell_with_input(input, 55);
        assert!(
            output.contains("put <key> <value>"),
            "should list put: {}",
            output
        );
        assert!(output.contains("get <key>"), "should list get: {}", output);
        assert!(
            output.contains("begin <name>"),
            "should list begin: {}",
            output
        );
    }

    #[test]
    fn shell_unknown_command() {
        let input = "foobar\nexit\n";
        let output = run_shell_with_input(input, 56);
        assert!(output.contains("error:"), "should show error: {}", output);
        assert!(
            output.contains("unknown command"),
            "should say unknown: {}",
            output
        );
    }

    #[test]
    fn shell_commit_no_active_txn() {
        let input = "commit\nexit\n";
        let output = run_shell_with_input(input, 57);
        assert!(output.contains("error:"), "should show error: {}", output);
        assert!(
            output.contains("no active transaction"),
            "should say no active txn: {}",
            output
        );
    }

    #[test]
    fn shell_scan_returns_all_keys() {
        let input = "put a 1\nput b 2\nput c 3\nscan\nexit\n";
        let output = run_shell_with_input(input, 58);
        assert!(output.contains("a = 1"), "should contain a = 1: {}", output);
        assert!(output.contains("b = 2"), "should contain b = 2: {}", output);
        assert!(output.contains("c = 3"), "should contain c = 3: {}", output);
    }

    #[test]
    fn shell_scan_with_bounds() {
        let input = "put a 1\nput b 2\nput c 3\nput d 4\nscan b d\nexit\n";
        let output = run_shell_with_input(input, 59);
        // scan b d means [b, d) - includes b and c, excludes d
        assert!(
            !output.contains("a = 1"),
            "should NOT contain a = 1: {}",
            output
        );
        assert!(output.contains("b = 2"), "should contain b = 2: {}", output);
        assert!(output.contains("c = 3"), "should contain c = 3: {}", output);
        assert!(
            !output.contains("d = 4"),
            "should NOT contain d = 4: {}",
            output
        );
    }
}
