// Ignore broken log pipes so a failed write cannot terminate a long-lived service.
#[macro_export]
macro_rules! safe_println {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let line = crate::activity::redact_log_line(&format!($($arg)*));
        let _ = writeln!(std::io::stdout(), "{line}");
    }};
}

#[macro_export]
macro_rules! safe_eprintln {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let line = crate::activity::redact_log_line(&format!($($arg)*));
        let _ = writeln!(std::io::stderr(), "{line}");
    }};
}

// All diagnostic println/eprintln calls share the token mask. MCP stdio responses
// write directly to their protocol writer rather than these logging macros.
macro_rules! println {
    () => { crate::safe_println!("") };
    ($($arg:tt)*) => { crate::safe_println!($($arg)*) };
}
macro_rules! eprintln {
    () => { crate::safe_eprintln!("") };
    ($($arg:tt)*) => { crate::safe_eprintln!($($arg)*) };
}
