/* vendored from ~/projects/uikit@d8803de — 原件改了这里跟着拷；生成物请跑 uikit 的 gen/gen.py */
// uikit panel — 轮询 / DOM / 格式化小助手，零依赖、无构建。
// 面板各自的布局与业务渲染留在各自项目里；这里只放三端重复的部分。

(function () {
  "use strict";

  /** 周期轮询。立即执行一次；回调抛错/请求失败不中断循环。
   *  poll("/api/status", render, 3000) */
  function poll(url, cb, intervalMs = 3000) {
    let stopped = false;
    async function tick() {
      if (stopped) return;
      try {
        const r = await fetch(url);
        await cb(await r.json());
      } catch (e) {
        /* 静默：下一轮再试。面板级错误展示由 cb 自己做 */
      }
      if (!stopped) setTimeout(tick, intervalMs);
    }
    tick();
    return () => (stopped = true);
  }

  /** 极小 DOM 构造器：el("div", {class:"card", onclick}, child, "text") */
  function el(tag, attrs, ...children) {
    const node = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs || {})) {
      if (k.startsWith("on") && typeof v === "function") node.addEventListener(k.slice(2), v);
      else if (v !== null && v !== undefined) node.setAttribute(k, v);
    }
    for (const c of children.flat()) {
      node.append(c instanceof Node ? c : document.createTextNode(String(c)));
    }
    return node;
  }

  /** 延迟毫秒 → "462ms"；超 1s → "1.2s" */
  function fmtMs(v) {
    if (v === null || v === undefined || !isFinite(v)) return "—";
    return v >= 1000 ? (v / 1000).toFixed(1) + "s" : Math.round(v) + "ms";
  }

  /** 秒 → "3s 前" / "5m 前" / "2h 前" */
  function fmtAgo(secs) {
    if (secs === undefined || secs === null || secs >= 1e9) return "—";
    if (secs >= 3600) return Math.floor(secs / 3600) + "h 前";
    if (secs >= 120) return Math.floor(secs / 60) + "m 前";
    return Math.floor(secs) + "s 前";
  }

  /** 延迟 → 语义色档（好/中/差），阈值可用 opts 覆盖 */
  function latencyTier(ms, okAt = 300, warnAt = 800) {
    if (ms === null || ms === undefined || !isFinite(ms)) return "dim";
    if (ms <= okAt) return "ok";
    if (ms <= warnAt) return "warn";
    return "bad";
  }

  window.UKPanel = { poll, el, fmtMs, fmtAgo, latencyTier };
})();
