(function () {
  "use strict";

  const norm = (s) =>
    (s || "").normalize("NFD").replace(/[̀-ͯ]/g, "").toLowerCase();

  // --- Home: live sort + filter over the rendered cards (no fetch needed),
  //     revealing results in pages so a big catalog stays light. ---
  const list = document.getElementById("cards");
  if (list) {
    const search = document.getElementById("search");
    const sort = document.getElementById("sort");
    const order = document.getElementById("order");
    const typeSel = document.getElementById("type");
    const noResults = document.getElementById("no-results");
    const moreBtn = document.getElementById("more");
    const cards = Array.from(list.children);
    const PAGE = 60;
    let shown = PAGE;

    function apply(resetPage) {
      if (resetPage) shown = PAGE;

      const key = sort.value;
      const dir = order.value === "asc" ? 1 : -1;
      const sorted = cards.slice().sort((a, b) => {
        let av, bv;
        if (key === "title") {
          av = norm(a.dataset.title);
          bv = norm(b.dataset.title);
        } else {
          av = a.dataset[key] || "";
          bv = b.dataset[key] || "";
        }
        if (av < bv) return -dir;
        if (av > bv) return dir;
        return 0;
      });
      sorted.forEach((c) => list.appendChild(c));

      const q = norm(search.value.trim());
      const type = typeSel.value;
      const matched = sorted.filter(
        (c) =>
          (type === "all" || c.dataset.type === type) &&
          (!q || norm(c.dataset.title).includes(q))
      );

      cards.forEach((c) => (c.hidden = true));
      matched.slice(0, shown).forEach((c) => (c.hidden = false));

      noResults.hidden = matched.length !== 0;
      if (moreBtn) moreBtn.hidden = matched.length <= shown;
    }

    [search, sort, order, typeSel].forEach((el) =>
      el.addEventListener("input", () => apply(true))
    );
    if (moreBtn) {
      moreBtn.addEventListener("click", () => {
        shown += PAGE;
        apply(false);
      });
    }
    apply(true);
  }

  // --- Detail: copy Rucio links, scoped to each release block ---
  async function copyText(text) {
    try {
      if (navigator.clipboard && window.isSecureContext) {
        await navigator.clipboard.writeText(text);
        return true;
      }
    } catch (e) {
      /* fall through to the legacy path (file://, insecure context) */
    }
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.setAttribute("readonly", "");
    ta.style.position = "fixed";
    ta.style.opacity = "0";
    document.body.appendChild(ta);
    ta.select();
    let ok = false;
    try {
      ok = document.execCommand("copy");
    } catch (e) {
      ok = false;
    }
    document.body.removeChild(ta);
    return ok;
  }

  document.querySelectorAll(".release").forEach((block) => {
    block.querySelectorAll("[data-copy]").forEach((btn) => {
      btn.addEventListener("click", async () => {
        let checks = Array.from(block.querySelectorAll(".link-check"));
        if (btn.dataset.copy === "selected") {
          checks = checks.filter((c) => c.checked);
        }
        const urls = checks.map((c) => c.dataset.url);
        if (!urls.length) return;

        if (await copyText(urls.join("\n"))) {
          const old = btn.textContent;
          btn.textContent = btn.dataset.copied || "OK";
          btn.classList.add("ok");
          setTimeout(() => {
            btn.textContent = old;
            btn.classList.remove("ok");
          }, 1400);
        }
      });
    });
  });

  // --- About modal: stats + TMDB attribution, from any page's footer ---
  const aboutModal = document.getElementById("about-modal");
  if (aboutModal && typeof aboutModal.showModal === "function") {
    document.querySelectorAll("[data-about-open]").forEach((btn) =>
      btn.addEventListener("click", () => aboutModal.showModal())
    );
    aboutModal.querySelectorAll("[data-about-close]").forEach((btn) =>
      btn.addEventListener("click", () => aboutModal.close())
    );
    // A click on the backdrop (the dialog itself, outside the box) closes it.
    aboutModal.addEventListener("click", (e) => {
      if (e.target === aboutModal) aboutModal.close();
    });
  }
})();
