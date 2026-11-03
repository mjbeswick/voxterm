use std::env;
use std::process::ExitCode;

/// Gemini key comes from the environment only. Never logged or printed.
fn gemini_key() -> Result<String, String> {
    match env::var("GEMINI_API_KEY") {
        Ok(k) if !k.trim().is_empty() => Ok(k),
        _ => Err("GEMINI_API_KEY is not set".into()),
    }
}

fn main() -> ExitCode {
    match gemini_key() {
        Ok(_) => {
            println!("voxterm: Gemini key found. Step 1 (ACP bridge) next.");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("voxterm: {e}");
            ExitCode::FAILURE
        }
    }
}
