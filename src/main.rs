// 地基阶段：核心类型尚未被运行时消费，暂允许 dead_code
#![allow(dead_code)]

mod config;
mod types;

use config::Config;

fn main() {
    println!("R2 Agent — booting…");
    let config = Config::default_config();
    println!("{:#?}", config);
}
