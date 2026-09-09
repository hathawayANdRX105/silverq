//! 子命令解析。
//!
//!   silverq                    # = serve（默认 nodes.yaml）
//!   silverq serve [nodes.yaml]
//!   silverq reload [nodes.yaml]
//!   silverq select <tag|auto>
use std::process;

pub enum Cmd {
    Serve {
        nodes: String,
        config: Option<String>,
    },
    Reload {
        nodes: Option<String>,
    },
    Select {
        tag: String,
    },
    Status,
}
impl Cmd {
    /// 对应的 ctl 命令文本（发给 daemon 的单行协议）。
    fn wire(&self) -> String {
        match self {
            Cmd::Reload { nodes } => match nodes {
                Some(p) => format!("reload {p}"),
                None => "reload".into(),
            },
            Cmd::Select { tag } => format!("select {tag}"),
            Cmd::Status => "status".into(),
            Cmd::Serve { .. } => unreachable!("serve 不走 ctl"),
        }
    }
}

pub fn parse() -> Cmd {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("serve") => {
            // serve [nodes.yaml] [--config silverq.toml]
            let nodes = args
                .iter()
                .skip(1)
                .find(|a| !a.starts_with("--") && !a.ends_with(".toml"))
                .cloned()
                .unwrap_or_else(|| "nodes.yaml".into());
            let config = args
                .iter()
                .skip(1)
                .position(|a| a == "--config")
                .and_then(|i| args.get(i + 2))
                .cloned();
            Cmd::Serve { nodes, config }
        }
        Some("reload") => Cmd::Reload {
            nodes: args.get(1).cloned(),
        },
        Some("status") => Cmd::Status,
        Some("select") => match args.get(1) {
            Some(tag) => Cmd::Select { tag: tag.clone() },
            None => die("select: 用法: silverq select <tag|auto>"),
        },
        Some(other) if other.starts_with("--") => die(&format!("未知选项: {other}")),
        Some(first) => {
            // 无子命令：第一个参数视为 nodes.yaml（serve 简写）
            if first.starts_with('.') || first.ends_with(".yaml") || first.ends_with(".yml") {
                Cmd::Serve {
                    nodes: first.to_string(),
                    config: None,
                }
            } else {
                die(&format!("未知命令: {first}（serve | reload | select）"))
            }
        }
        None => Cmd::Serve {
            nodes: "nodes.yaml".into(),
            config: None,
        },
    }
}

fn die(msg: &str) -> ! {
    eprintln!("silverq: {msg}");
    process::exit(2);
}

pub fn ctl_line(cmd: &Cmd) -> String {
    cmd.wire()
}
