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

    // --- Facets: navigate by clicking a chip (on a card or in the facet bar).
    //     Each chip is a facet value ("tag" genres/labels, "res" quality).
    //     Within a facet the active values combine with OR (broaden); facets
    //     combine with AND (narrow); the text and type filters apply on top. ---
    const fkey = (facet, val) => facet + "\u0000" + val;
    const active = new Set(); // "facet\u0000val" keys currently selected
    const cardFacets = new Map();
    cards.forEach((c) =>
      cardFacets.set(
        c,
        new Set(
          Array.from(c.querySelectorAll("[data-facet]")).map((e) =>
            fkey(e.dataset.facet, e.dataset.val)
          )
        )
      )
    );

    // Quality sorts best-first; unknown values fall to the end, then A–Z.
    const RES_RANK = { "4K": 0, "1080p": 1, "720p": 2 };
    const cmpFor = (facet) =>
      facet === "res"
        ? (a, b) =>
            (RES_RANK[a] ?? 9) - (RES_RANK[b] ?? 9) || a.localeCompare(b)
        : (a, b) => a.localeCompare(b);

    // Fill a bar group with a button per distinct value of a facet; returns
    // whether any value existed (so empty groups can hide).
    function buildGroup(facet, container, chipClass) {
      if (!container) return false;
      const vals = Array.from(
        new Set(
          [].concat(
            ...cards.map((c) =>
              Array.from(
                c.querySelectorAll('[data-facet="' + facet + '"]')
              ).map((e) => e.dataset.val)
            )
          )
        )
      ).sort(cmpFor(facet));
      vals.forEach((val) => {
        const b = document.createElement("button");
        b.type = "button";
        b.className = chipClass;
        b.dataset.facet = facet;
        b.dataset.val = val;
        b.textContent = val;
        container.appendChild(b);
      });
      return vals.length > 0;
    }

    const facetbar = document.getElementById("facetbar");
    const hasTags = buildGroup("tag", document.getElementById("tagchips"), "tag");
    const hasRes = buildGroup(
      "res",
      document.getElementById("reschips"),
      "chip chip-res"
    );
    document.querySelectorAll(".facet-group").forEach((g) => {
      g.hidden = !g.querySelector("[data-facet]");
    });
    if (facetbar) facetbar.hidden = !(hasTags || hasRes);

    function syncFacetState() {
      document
        .querySelectorAll("[data-facet]")
        .forEach((el) =>
          el.classList.toggle(
            "active",
            active.has(fkey(el.dataset.facet, el.dataset.val))
          )
        );
    }

    function toggleFacet(facet, val) {
      const k = fkey(facet, val);
      if (active.has(k)) active.delete(k);
      else active.add(k);
      syncFacetState();
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

      // Group the active values by facet for OR-within / AND-across matching.
      const byFacet = new Map();
      for (const k of active) {
        const sep = k.indexOf("\u0000");
        const facet = k.slice(0, sep);
        if (!byFacet.has(facet)) byFacet.set(facet, []);
        byFacet.get(facet).push(k);
      }

      const q = norm(search.value.trim());
      const type = typeSel.value;
      const matched = sorted.filter((c) => {
        if (type !== "all" && c.dataset.type !== type) return false;
        if (q && !norm(c.dataset.title).includes(q)) return false;
        const facets = cardFacets.get(c);
        for (const keys of byFacet.values()) {
          if (!keys.some((k) => facets.has(k))) return false;
        }
        return true;
      });

      cards.forEach((c) => (c.hidden = true));
      matched.slice(0, shown).forEach((c) => (c.hidden = false));

      noResults.hidden = matched.length !== 0;
      if (moreBtn) moreBtn.hidden = matched.length <= shown;
    }

    // A click on any facet chip (card or bar) drives the filter instead of
    // following the card link.
    document.addEventListener("click", (e) => {
      const chip = e.target.closest("[data-facet]");
      if (!chip) return;
      e.preventDefault();
      e.stopPropagation();
      toggleFacet(chip.dataset.facet, chip.dataset.val);
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
