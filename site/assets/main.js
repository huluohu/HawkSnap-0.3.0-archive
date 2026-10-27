/* HawkSnap product site — 零依赖交互：主题切换 / 平滑滚动 / 复制按钮 / 页脚年份。
   纯原生 JS（无框架、无构建），defer 加载，DOM 就绪后才执行。 */
(function () {
  "use strict";

  /* ---------- 1. 主题切换（localStorage 持久化；初始值由 <head> 内联脚本预置） ---------- */
  var THEME_KEY = "hawksnap-theme";
  var root = document.documentElement;

  function currentTheme() {
    return root.getAttribute("data-theme") === "light" ? "light" : "dark";
  }

  function applyTheme(theme) {
    root.setAttribute("data-theme", theme);
    try { localStorage.setItem(THEME_KEY, theme); } catch (e) { /* 隐私模式等场景：仅本次会话生效 */ }
  }

  var themeButtons = document.querySelectorAll("[data-theme-toggle]");
  themeButtons.forEach(function (btn) {
    btn.addEventListener("click", function () {
      applyTheme(currentTheme() === "dark" ? "light" : "dark");
    });
  });

  // 未做过手动选择时跟随系统变化（做过选择则尊重用户，localStorage 优先）。
  if (window.matchMedia) {
    var mq = window.matchMedia("(prefers-color-scheme: light)");
    var onChange = function (ev) {
      var stored = null;
      try { stored = localStorage.getItem(THEME_KEY); } catch (e) { /* ignore */ }
      if (stored !== "light" && stored !== "dark") {
        root.setAttribute("data-theme", ev.matches ? "light" : "dark");
      }
    };
    if (mq.addEventListener) mq.addEventListener("change", onChange);
    else if (mq.addListener) mq.addListener(onChange);
  }

  /* ---------- 2. 平滑滚动 ---------- */
  // 滚动本身交给 CSS（html { scroll-behavior: smooth } + scroll-padding-top 补偿粘性头部）；
  // JS 负责在用户偏好「减少动态效果」时禁用平滑滚动（CSS @media 已兜底，这里同步内联样式）。
  if (window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
    root.style.scrollBehavior = "auto";
  }

  /* ---------- 3. 复制按钮（安装命令 / 代码块） ---------- */
  // 约定：<pre data-copyable><code>…</code></pre> 自动注入右上角复制按钮。
  function copyText(text, done) {
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(done, function () { legacyCopy(text, done); });
    } else {
      legacyCopy(text, done);
    }
  }

  // 降级：隐藏 textarea + execCommand（file:// 下 clipboard API 可能不可用）。
  function legacyCopy(text, done) {
    var ta = document.createElement("textarea");
    ta.value = text;
    ta.setAttribute("readonly", "");
    ta.style.position = "fixed";
    ta.style.left = "-9999px";
    document.body.appendChild(ta);
    ta.select();
    try { document.execCommand("copy"); } catch (e) { /* 尽力而为 */ }
    document.body.removeChild(ta);
    done();
  }

  document.querySelectorAll("pre[data-copyable]").forEach(function (pre) {
    var code = pre.querySelector("code");
    if (!code) return;
    var btn = document.createElement("button");
    btn.type = "button";
    btn.className = "copy-btn";
    btn.textContent = "复制";
    btn.setAttribute("aria-label", "复制代码块");
    btn.addEventListener("click", function () {
      copyText(code.textContent, function () {
        btn.textContent = "已复制";
        btn.classList.add("is-copied");
        window.setTimeout(function () {
          btn.textContent = "复制";
          btn.classList.remove("is-copied");
        }, 1600);
      });
    });
    pre.appendChild(btn);
  });

  /* ---------- 4. 页脚年份 ---------- */
  document.querySelectorAll("[data-year]").forEach(function (el) {
    el.textContent = String(new Date().getFullYear());
  });
})();
