#![cfg(feature = "meow")]

use silverq::web::*;

/// 引导脚本只注入 index.html 的 </body> 前，且无 </body> 时原样返回。
#[test]
fn inject_bootstrap_appends_before_body_close() {
    let html = "<html><body>x</body></html>";
    let out = inject_bootstrap(html);
    assert!(out.contains("<script>"), "必须注入 script");
    assert!(
        out.contains("setup/api-list"),
        "必须按后端列表判空，避免首访后引导失效"
    );
    assert!(out.ends_with("</body></html>"), "注入点必须在 </body> 前");
    // 无 </body> 的输入原样返回（真实 index.html 恒有 </body>）
    assert_eq!(inject_bootstrap("<html>"), "<html>");
}

/// 调参夹紧：面板/ PATCH 传任意值都不会把调度打坏。
#[test]
fn tuning_validation_clamps() {
    use silverq::config::settings::RuntimeTuning;
    let t = RuntimeTuning {
        capacity: 9999,
        batch_size: 0,
        interval_secs: 1,
        timeout_ms: 10,
        concurrency: 0,
        timeout_penalty: -5.0,
        fallback_attempts: 100,
        retire_max_failures: 5000,
        retire_keep_alive_secs: 7200,
        retire_min_pool: 10,
        bw_interval_rounds: 0,
        bw_timeout_ms: 10,
        bw_max_bytes: 0,
        bw_penalty_per_efold_ms: -1.0,
    }
    .validated();
    assert_eq!(t.capacity, 50);
    assert_eq!(t.batch_size, 1);
    assert_eq!(t.interval_secs, 5);
    assert_eq!(t.timeout_ms, 500);
    assert_eq!(t.concurrency, 1);
    assert_eq!(t.timeout_penalty, 100.0);
    assert_eq!(t.retire_max_failures, 1000);
    assert_eq!(t.fallback_attempts, 10);
    assert_eq!(t.bw_interval_rounds, 1);
    assert_eq!(t.bw_timeout_ms, 1000);
    assert_eq!(t.bw_max_bytes, 16 * 1024);
    assert_eq!(t.bw_penalty_per_efold_ms, 0.0);
}

/// rfc3339 必须产出标准 ISO 时间，dayjs（zashboard 的解析器）才认。
#[test]
fn rfc3339_formats_known_epochs() {
    assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
    assert_eq!(rfc3339(86_400), "1970-01-02T00:00:00Z");
    // 2026-09-10T08:00:00Z == 1789027200（与 python 交叉核对）
    assert_eq!(rfc3339(1_789_027_200), "2026-09-10T08:00:00Z");
    // 闰年 2024-02-29
    assert_eq!(rfc3339(1_709_164_800), "2024-02-29T00:00:00Z");
}
