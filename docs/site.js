(() => {
  const reduced = window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  document.querySelectorAll("[data-copy-target]").forEach((btn) => {
    btn.addEventListener("click", async () => {
      const targetId = btn.getAttribute("data-copy-target");
      const el = targetId ? document.getElementById(targetId) : null;
      if (!el) return;
      const text = el.textContent.trim();
      const orig = btn.getAttribute("data-label") || btn.textContent;
      try {
        await navigator.clipboard.writeText(text);
        btn.setAttribute("data-copied", "true");
        btn.textContent = "Copied";
        window.setTimeout(() => {
          btn.removeAttribute("data-copied");
          btn.textContent = orig;
        }, 1600);
      } catch {
        // Fallback selection when clipboard permission is unavailable.
        const range = document.createRange();
        range.selectNodeContents(el);
        const sel = window.getSelection();
        if (sel) {
          sel.removeAllRanges();
          sel.addRange(range);
        }
      }
    });
  });

  if (!reduced) {
    document.querySelectorAll("[data-toggle-media]").forEach((btn) => {
      const imgId = btn.getAttribute("data-toggle-media");
      const img = imgId ? document.getElementById(imgId) : null;
      if (!img) return;
      const animatedSrc = img.getAttribute("data-animated-src");
      const staticSrc = img.getAttribute("data-static-src");
      if (!animatedSrc || !staticSrc) return;

      btn.addEventListener("click", () => {
        const paused = btn.getAttribute("aria-pressed") === "true";
        if (paused) {
          img.setAttribute("src", animatedSrc);
          btn.setAttribute("aria-pressed", "false");
          btn.textContent = "Still frame";
        } else {
          img.setAttribute("src", staticSrc);
          btn.setAttribute("aria-pressed", "true");
          btn.textContent = "Play capture";
        }
      });
    });
  }
})();
