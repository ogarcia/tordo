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

    // --- Tags: navigate the catalog by clicking a tag chip (on a card or in
    //     the tag bar). Active tags combine with AND; the text and type filters
    //     still apply on top. ---
    const active = new Set();
    const cardTags = new Map();
    cards.forEach((c) =>
      cardTags.set(
        c,
        new Set(Array.from(c.querySelectorAll(".tag")).map((t) => t.dataset.tag))
      )
    );

    // Build the tag bar from the union of every card's tags, sorted.
    const tagbar = document.getElementById("tagbar");
    const tagchips = document.getElementById("tagchips");
    if (tagbar && tagchips) {
      const all = Array.from(
        new Set([].concat(...cards.map((c) => Array.from(cardTags.get(c)))))
      ).sort((a, b) => a.localeCompare(b));
      all.forEach((tag) => {
        const b = document.createElement("button");
        b.type = "button";
        b.className = "tag";
        b.dataset.tag = tag;
        b.textContent = tag;
        tagchips.appendChild(b);
      });
      tagbar.hidden = all.length === 0;
    }

    function syncTagState() {
      document
        .querySelectorAll(".tag")
        .forEach((el) => el.classList.toggle("active", active.has(el.dataset.tag)));
    }

    function toggleTag(tag) {
      if (active.has(tag)) active.delete(tag);
      else active.add(tag);
      syncTagState();
      apply(true);
    }

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
      const matched = sorted.filter((c) => {
        if (type !== "all" && c.dataset.type !== type) return false;
        if (q && !norm(c.dataset.title).includes(q)) return false;
        for (const tag of active) {
          if (!cardTags.get(c).has(tag)) return false;
        }
        return true;
      });

      cards.forEach((c) => (c.hidden = true));
      matched.slice(0, shown).forEach((c) => (c.hidden = false));

      noResults.hidden = matched.length !== 0;
      if (moreBtn) moreBtn.hidden = matched.length <= shown;
    }

    // A click on any tag chip (card or bar) drives the filter instead of
    // following the card link.
    document.addEventListener("click", (e) => {
      const chip = e.target.closest(".tag");
      if (!chip) return;
      e.preventDefault();
      e.stopPropagation();
      toggleTag(chip.dataset.tag);
    });

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
