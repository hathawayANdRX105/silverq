/* vendored from ~/projects/uikit@d8803de — 原件改了这里跟着拷；生成物请跑 uikit 的 gen/gen.py */
// uikit charts — canvas 原生图表原语，零依赖、无构建。
// 消费方：sparkline(canvas, data, opts)。颜色从 tokens.css 的 CSS 变量现场取。

(function () {
  "use strict";

  function cssVar(name, fallback) {
    const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
    return v || fallback;
  }

  // 按 devicePixelRatio 把 canvas 物理分辨率对齐到 CSS 尺寸，避免模糊。
  // 返回 ctx（已 scale）与 CSS 像素下的 w/h。
  function setup(canvas) {
    const dpr = window.devicePixelRatio || 1;
    const w = canvas.clientWidth || 100;
    const h = canvas.clientHeight || 30;
    if (canvas.width !== Math.round(w * dpr) || canvas.height !== Math.round(h * dpr)) {
      canvas.width = Math.round(w * dpr);
      canvas.height = Math.round(h * dpr);
    }
    const ctx = canvas.getContext("2d");
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, w, h);
    return { ctx, w, h };
  }

  /**
   * sparkline(canvas, data, opts)
   *   data: number[]（如 EWMA 延迟毫秒序列，旧→新）
   *   opts.stroke:   CSS 颜色，默认 var(--acc)
   *   opts.fill:     填充色，默认 stroke 的 12% 透明叠加；false 关闭
   *   opts.pad:      上下留白 CSS px（默认 3）
   *   opts.dot:      末点圆点（默认 true）
   *   opts.baseline: 参考线数值（可选），超过它画 var(--bad) 色
   * 纵轴 min-max 自适应（延迟图比 0 基线更可读）；单点/空数据安全。
   */
  function sparkline(canvas, data, opts = {}) {
    if (!canvas || !Array.isArray(data)) return;
    const { ctx, w, h } = setup(canvas);
    const pad = opts.pad ?? 3;
    const stroke = opts.stroke || cssVar("--acc", "#5b9bd5");
    if (data.length === 0) return;

    const lo = Math.min(...data);
    const hi = Math.max(...data);
    const span = hi - lo || 1;
    const x = (i) => (data.length === 1 ? w / 2 : (i / (data.length - 1)) * (w - 2) + 1);
    const y = (v) => h - pad - ((v - lo) / span) * (h - pad * 2);

    // 超过 baseline 的段落用警示色重描——一眼看出劣化区间。
    let over = null;
    if (opts.baseline !== undefined) {
      over = data.some((v) => v > opts.baseline);
    }

    const path = () => {
      ctx.beginPath();
      data.forEach((v, i) => (i ? ctx.lineTo(x(i), y(v)) : ctx.moveTo(x(i), y(v))));
    };

    if (opts.fill !== false) {
      path();
      ctx.lineTo(x(data.length - 1), h);
      ctx.lineTo(x(0), h);
      ctx.closePath();
      ctx.globalAlpha = 0.12;
      ctx.fillStyle = over ? cssVar("--bad", "#e06c60") : stroke;
      ctx.fill();
      ctx.globalAlpha = 1;
    }

    path();
    ctx.strokeStyle = over ? cssVar("--bad", "#e06c60") : stroke;
    ctx.lineWidth = 1.5;
    ctx.lineJoin = "round";
    ctx.stroke();

    if (opts.dot !== false) {
      ctx.beginPath();
      ctx.arc(x(data.length - 1), y(data[data.length - 1]), 2, 0, Math.PI * 2);
      ctx.fillStyle = stroke;
      ctx.fill();
    }
  }

  window.UKCharts = { sparkline };
})();
