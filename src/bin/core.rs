//! core-rs 命令行（feature = "cli"）：
//! - `core-rs version`：打印版本；
//! - `core-rs hash <password>`：argon2 生成密码哈希（需 feature = "jwt"），
//!   用于运维侧预生成数据/种子脚本，避免在业务代码里临时写哈希逻辑。

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "core-rs", about = "core-rs 框架命令行")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 打印框架版本
    Version,
    /// argon2 生成密码哈希（feature = "jwt"）；省略 password 时从 stdin 读取，
    /// 避免明文密码留在 shell history / 进程列表
    #[cfg(feature = "jwt")]
    Hash { password: Option<String> },
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Version => {
            println!("core-rs {}", env!("CARGO_PKG_VERSION"));
        }
        #[cfg(feature = "jwt")]
        Command::Hash { password } => {
            let password = password.unwrap_or_else(read_password_from_stdin);
            let hashed = core_rs::security::password::hash(&password)
                .unwrap_or_else(|e| {
                    eprintln!("hash failed: {e}");
                    std::process::exit(1);
                });
            println!("{hashed}");
        }
    }
}

#[cfg(feature = "jwt")]
fn read_password_from_stdin() -> String {
    let mut buf = String::new();
    std::io::stdin()
        .read_line(&mut buf)
        .unwrap_or_else(|e| {
            eprintln!("read stdin failed: {e}");
            std::process::exit(1);
        });
    buf.trim_end_matches(['\r', '\n']).to_string()
}
