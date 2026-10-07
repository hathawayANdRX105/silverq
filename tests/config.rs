//! 探测目标列表解析（src/config/mod.rs 的 split_probe_urls 纯函数测试）。

use silverq::config::{split_probe_urls, DEFAULT_PROBE_URL};

#[test]
fn split_probe_urls_single() {
    assert_eq!(
        split_probe_urls("https://www.gstatic.com/generate_204"),
        vec!["https://www.gstatic.com/generate_204"]
    );
}

#[test]
fn split_probe_urls_multiple_trims_and_drops_empties() {
    // SNI 白名单检测的实际配置形态：两个目标，带空白和空项
    assert_eq!(
        split_probe_urls(" https://a/generate_204 , https://b/generate_204 , "),
        vec!["https://a/generate_204", "https://b/generate_204"]
    );
    // 中间空项必须丢弃，否则 measure() 会去拨空 URL，整池误判废
    assert_eq!(
        split_probe_urls("https://a/,,https://b/"),
        vec!["https://a/", "https://b/"]
    );
}

#[test]
fn split_probe_urls_empty_falls_back_to_default() {
    // 未设置 env 或写空：行为必须等价于旧的单目标配置
    assert_eq!(split_probe_urls(""), vec![DEFAULT_PROBE_URL]);
    assert_eq!(split_probe_urls("   "), vec![DEFAULT_PROBE_URL]);
    assert_eq!(split_probe_urls(",,,,"), vec![DEFAULT_PROBE_URL]);
}
