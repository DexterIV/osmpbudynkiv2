// Statistics page (stats.html): renders GET /stats and GET /stats/areas.
//
// The ranking's controls live in the URL hash
// (#level=settlement&sort=unmatched&order=asc&page=3) so a ranking can be
// shared. Unlike the map's view options, which deliberately stay out of the
// hash, this is a separate page describing what to look at.
(function () {
  "use strict";

  const PER_PAGE = 50;
  const DEFAULTS = { level: "gmina", sort: "ratio", order: "desc", page: 1 };
  const ALLOWED = {
    level: ["gmina", "settlement"],
    sort: ["ratio", "unmatched"],
    order: ["desc", "asc"],
  };

  const SOURCE_LABELS = {
    prg: "Adresy (PRG)",
    bdot10k: "Budynki (BDOT10k)",
    egib: "Budynki (EGIB)",
  };
  const FRESHNESS_LABELS = {
    prg: "PRG",
    bdot10k: "BDOT10k",
    egib: "EGIB",
  };
  const OSM_TYPE_LABELS = { node: "punkty", way: "linie (way)", relation: "relacje" };
  const REPORT_STATUSES = ["active", "revoked", "expired"];
  // The last digit of a 7-character TERC gmina code.
  const GMINA_TYPES = { 1: "gmina miejska", 2: "gmina wiejska", 3: "gmina miejsko-wiejska" };

  // "always": Polish formatting groups only from five digits up (CLDR
  // minimumGroupingDigits = 2), so 3665 would sit bare next to 17 989 407 in
  // the same column. A browser predating the option reads it as `true` and
  // falls back to that locale default.
  const numberFormat = new Intl.NumberFormat("pl-PL", { useGrouping: "always" });
  const percentFormat = new Intl.NumberFormat("pl-PL", {
    minimumFractionDigits: 1,
    maximumFractionDigits: 1,
  });

  const $ = (id) => document.getElementById(id);

  // The map's own zoom range (minZoom/maxZoom in app.js); the upper bound is
  // kept below it so a one-address area opens at street level, not at the
  // map's deepest zoom.
  const MAP_MIN_ZOOM = 5;
  const MAP_MAX_ZOOM = 17;
  // MapLibre's world is 512 CSS px wide at zoom 0.
  const WORLD_PX = 512;
  // Share of the viewport the extent may fill, leaving a margin around it.
  const FIT_FRACTION = 0.85;

  function fmt(n) {
    return numberFormat.format(n);
  }

  function pct(part, whole) {
    return whole > 0 ? (100 * part) / whole : 0;
  }

  function fmtDate(iso) {
    return iso ? new Date(iso).toLocaleString("pl-PL") : "—";
  }

  function fmtLag(seconds) {
    if (seconds == null) return "";
    if (seconds < 120) return `${seconds} s`;
    const minutes = Math.round(seconds / 60);
    if (minutes < 120) return `${minutes} min`;
    const hours = Math.round(minutes / 60);
    if (hours < 48) return `${hours} h`;
    return `${Math.round(hours / 24)} dni`;
  }

  /** A table row from cell specs: a string is a text cell, `{num: n}` a
   *  right-aligned number, `{node}` any element, `{text}` text, and `th: true`
   *  on the first cell makes it the row header. `className` applies to
   *  `{node}` and `{text}` cells. */
  function row(cells, className) {
    const tr = document.createElement("tr");
    if (className) tr.className = className;
    cells.forEach((cell, i) => {
      const td = document.createElement(i === 0 && cell && cell.th ? "th" : "td");
      if (i === 0 && cell && cell.th) td.scope = "row";
      if (cell && typeof cell === "object") {
        if (cell.num !== undefined) {
          td.className = "num";
          td.textContent = fmt(cell.num);
        } else if (cell.node) {
          if (cell.className) td.className = cell.className;
          td.append(cell.node);
        } else {
          if (cell.className) td.className = cell.className;
          td.textContent = cell.text;
        }
      } else {
        td.textContent = cell;
      }
      tr.append(td);
    });
    return tr;
  }

  function ratioNode(part, whole) {
    const value = pct(part, whole);
    const wrap = document.createElement("span");
    wrap.className = "stats-ratio";
    const bar = document.createElement("span");
    bar.className = "stats-bar";
    bar.setAttribute("aria-hidden", "true");
    const fill = document.createElement("span");
    fill.style.width = `${Math.min(100, value)}%`;
    bar.append(fill);
    const text = document.createElement("span");
    text.textContent = `${percentFormat.format(value)} %`;
    wrap.append(bar, text);
    return wrap;
  }

  function showError(message) {
    const el = $("stats-error");
    el.textContent = message;
    el.hidden = false;
  }

  async function fetchJson(url) {
    const response = await fetch(url);
    if (!response.ok) {
      let detail = "";
      try {
        detail = (await response.json()).error || "";
      } catch (e) {
        /* Not JSON: the status line is all there is. */
      }
      throw new Error(`${url}: ${response.status} ${detail}`.trim());
    }
    return response.json();
  }

  // --- Summary -----------------------------------------------------------

  function renderSummary(stats) {
    $("computed-at").textContent = `Stan na ${fmtDate(stats.computed_at)}`;

    // Last successful load: the later of an import and a refresh.
    const freshness = $("freshness-body");
    freshness.replaceChildren();
    for (const d of stats.datasets) {
      const imported = d.imported_at ? Date.parse(d.imported_at) : -Infinity;
      const refreshed = d.refreshed_at ? Date.parse(d.refreshed_at) : -Infinity;
      let when = null;
      let how = "brak danych";
      if (imported > -Infinity || refreshed > -Infinity) {
        when = imported >= refreshed ? d.imported_at : d.refreshed_at;
        how = imported >= refreshed ? "import" : "aktualizacja";
      }
      freshness.append(row([{ th: true, text: FRESHNESS_LABELS[d.source] || d.source }, fmtDate(when), how]));
    }
    const osm = stats.osm;
    const lag = osm.lag_seconds != null ? ` (opóźnienie ${fmtLag(osm.lag_seconds)})` : "";
    freshness.append(
      row([{ th: true, text: "OSM" }, fmtDate(osm.replication_timestamp), `replikacja${lag}`]),
    );

    const reports = $("reports-body");
    reports.replaceChildren();
    const bySource = {};
    for (const r of stats.reports) {
      (bySource[r.source] ||= {})[r.status] = r.count;
    }
    const totals = { active: 0, revoked: 0, expired: 0, all: 0 };
    for (const d of stats.datasets) {
      const counts = bySource[d.source] || {};
      const values = REPORT_STATUSES.map((s) => counts[s] || 0);
      const sum = values.reduce((a, b) => a + b, 0);
      REPORT_STATUSES.forEach((s, i) => (totals[s] += values[i]));
      totals.all += sum;
      reports.append(
        row([{ th: true, text: FRESHNESS_LABELS[d.source] || d.source }, ...values.map((n) => ({ num: n })), { num: sum }]),
      );
    }
    reports.append(
      row([{ th: true, text: "Razem" }, ...REPORT_STATUSES.map((s) => ({ num: totals[s] })), { num: totals.all }], "total"),
    );

    renderTypeCounts($("osm-addresses-body"), osm.addresses);
    const buildings = $("osm-buildings-body");
    renderTypeCounts(buildings, osm.buildings);
    buildings.append(row([{ th: true, text: "dawne (demolished:building itp.)" }, { num: osm.former_buildings }]));

    const values = $("osm-values-body");
    values.replaceChildren();
    for (const v of osm.top_building_values) {
      const code = document.createElement("code");
      code.textContent = v.value ?? "(brak)";
      values.append(row([{ th: true, node: code }, { num: v.count }]));
    }

    const datasets = $("datasets-body");
    datasets.replaceChildren();
    for (const d of stats.datasets) {
      datasets.append(
        row([
          { th: true, text: SOURCE_LABELS[d.source] || d.source },
          { num: d.comparable },
          { num: d.unmatched },
          { node: ratioNode(d.unmatched, d.comparable), className: "num" },
        ]),
      );
    }
  }

  /** Counts keyed by OSM type, in node/way/relation order, then a total. */
  function renderTypeCounts(tbody, counts) {
    tbody.replaceChildren();
    let total = 0;
    const keys = Object.keys(counts).sort(
      (a, b) => Object.keys(OSM_TYPE_LABELS).indexOf(a) - Object.keys(OSM_TYPE_LABELS).indexOf(b),
    );
    for (const key of keys) {
      total += counts[key];
      tbody.append(row([{ th: true, text: OSM_TYPE_LABELS[key] || key }, { num: counts[key] }]));
    }
    tbody.append(row([{ th: true, text: "razem" }, { num: total }], "total"));
  }

  // --- Map link ----------------------------------------------------------

  function mercatorY(lat) {
    return Math.log(Math.tan(Math.PI / 4 + (lat * Math.PI) / 360));
  }

  /** A link to the map fitted to `bbox` ([min lon, min lat, max lon, max
   *  lat]), in the `map=zoom/lat/lon` hash MapLibre reads (`hash: "map"` in
   *  app.js). The fit assumes the map opens in a window this size. Relative,
   *  so it works under any path prefix. */
  function mapHref(bbox) {
    const [x0, y0, x1, y1] = bbox;
    const width = Math.max(320, window.innerWidth) * FIT_FRACTION;
    const height = Math.max(320, window.innerHeight) * FIT_FRACTION;
    const spanX = ((x1 - x0) * Math.PI) / 180;
    const spanY = mercatorY(y1) - mercatorY(y0);
    // A zero span (one address, or a row of them) fits any zoom.
    const fit = (px, span) => (span > 0 ? Math.log2((px * 2 * Math.PI) / (WORLD_PX * span)) : Infinity);
    const zoom = Math.min(MAP_MAX_ZOOM, Math.max(MAP_MIN_ZOOM, Math.min(fit(width, spanX), fit(height, spanY))));
    const midY = (mercatorY(y0) + mercatorY(y1)) / 2;
    const lat = (Math.atan(Math.sinh(midY)) * 180) / Math.PI;
    const lon = (x0 + x1) / 2;
    return `./#map=${zoom.toFixed(2)}/${lat.toFixed(5)}/${lon.toFixed(5)}`;
  }

  function mapLinkCell(item) {
    if (!item.bbox) return "";
    const a = document.createElement("a");
    a.className = "stats-map-link";
    a.href = mapHref(item.bbox);
    a.textContent = "Mapa";
    a.setAttribute("aria-label", `Pokaż ${item.name} na mapie`);
    return { node: a };
  }

  // --- Ranking -----------------------------------------------------------

  function readHash() {
    const params = new URLSearchParams(location.hash.slice(1));
    const state = { ...DEFAULTS };
    for (const key of Object.keys(ALLOWED)) {
      const value = params.get(key);
      if (ALLOWED[key].includes(value)) state[key] = value;
    }
    const page = Number.parseInt(params.get("page"), 10);
    if (Number.isFinite(page) && page >= 1) state.page = page;
    return state;
  }

  function writeHash(state) {
    const params = new URLSearchParams();
    for (const key of Object.keys(DEFAULTS)) {
      if (String(state[key]) !== String(DEFAULTS[key])) params.set(key, state[key]);
    }
    const hash = params.toString();
    // replaceState, not location.hash: the ranking's controls are not
    // history entries, and "← Mapa" relies on history.back() landing on the map.
    history.replaceState(null, "", hash ? `#${hash}` : location.pathname + location.search);
  }

  let current = readHash();
  let requestSerial = 0;

  function syncControls(state) {
    const form = $("areas-controls");
    for (const key of Object.keys(ALLOWED)) form.elements[key].value = state[key];
  }

  function nameCell(item, level) {
    const wrap = document.createElement("span");
    wrap.textContent = item.name || "(bez nazwy)";
    if (level === "gmina") {
      const type = GMINA_TYPES[item.teryt.slice(-1)];
      if (type) {
        const sub = document.createElement("span");
        sub.className = "stats-sub";
        sub.textContent = ` ${type}`;
        wrap.append(sub);
      }
    }
    return wrap;
  }

  function gminaCell(item) {
    const wrap = document.createElement("span");
    wrap.textContent = item.gmina;
    const type = GMINA_TYPES[item.teryt_gmina.slice(-1)];
    if (type) {
      const sub = document.createElement("span");
      sub.className = "stats-sub";
      sub.textContent = ` ${type.replace("gmina ", "")}`;
      wrap.append(sub);
    }
    return wrap;
  }

  async function loadAreas() {
    const state = current;
    const serial = ++requestSerial;
    const query = new URLSearchParams({
      level: state.level,
      sort: state.sort,
      order: state.order,
      page: String(state.page),
      per_page: String(PER_PAGE),
    });
    let data;
    try {
      data = await fetchJson(`stats/areas?${query}`);
    } catch (e) {
      showError(`Nie udało się wczytać rankingu: ${e.message}`);
      return;
    }
    // A slower earlier response must not overwrite a newer one.
    if (serial !== requestSerial) return;

    const settlement = state.level === "settlement";
    $("areas-name-col").textContent = settlement ? "Miejscowość" : "Gmina";
    // A settlement's own TERYT code is its SIMC identifier, a gmina's its TERC.
    $("areas-code-col").textContent = settlement ? "SIMC" : "TERC";
    $("areas-gmina-col").hidden = !settlement;
    $("areas-gmina-code-col").hidden = !settlement;

    const tbody = $("areas-body");
    tbody.replaceChildren();
    const offset = (data.page - 1) * data.per_page;
    data.items.forEach((item, i) => {
      const cells = [
        { num: offset + i + 1 },
        { node: nameCell(item, state.level) },
        { text: item.teryt || "—", className: "code" },
      ];
      if (settlement) {
        cells.push({ node: gminaCell(item) }, { text: item.teryt_gmina || "—", className: "code" });
      }
      cells.push(
        { num: item.total },
        { num: item.unmatched },
        { node: ratioNode(item.unmatched, item.total), className: "num" },
        mapLinkCell(item),
      );
      tbody.append(row(cells));
    });

    const pages = Math.max(1, Math.ceil(data.total_items / data.per_page));
    $("page-info").textContent = `Strona ${fmt(data.page)} z ${fmt(pages)} (${fmt(data.total_items)} ${settlement ? "miejscowości" : "gmin"})`;
    $("prev-page").disabled = data.page <= 1;
    $("next-page").disabled = data.page >= pages;
  }

  function update(changes) {
    current = { ...current, ...changes };
    writeHash(current);
    syncControls(current);
    loadAreas();
  }

  // --- Theme -------------------------------------------------------------
  //
  // The same rule as app.js's #theme-toggle, minus the map: a choice is stored
  // only while it differs from the system setting, and the OS switching theme
  // is followed only while nothing is stored. The key is shared, so a choice
  // made here holds on the map and back.
  const THEME_STORAGE_KEY = "osmpbudynkiv2.theme"; // also read by stats.html's <head> script
  const themeToggle = $("theme-toggle");
  const systemDarkQuery = window.matchMedia("(prefers-color-scheme: dark)");

  function systemTheme() {
    return systemDarkQuery.matches ? "dark" : "light";
  }

  function storedTheme() {
    try {
      const theme = localStorage.getItem(THEME_STORAGE_KEY);
      return theme === "light" || theme === "dark" ? theme : null;
    } catch {
      return null;
    }
  }

  function storeTheme(theme) {
    try {
      if (theme === systemTheme()) localStorage.removeItem(THEME_STORAGE_KEY);
      else localStorage.setItem(THEME_STORAGE_KEY, theme);
    } catch {
      /* Preference is not persisted; the session still works. */
    }
  }

  function setTheme(theme) {
    document.documentElement.dataset.theme = theme;
    const dark = theme === "dark";
    themeToggle.setAttribute("aria-pressed", String(dark));
    themeToggle.title = dark ? "Przełącz na tryb jasny" : "Przełącz na tryb ciemny";
  }

  themeToggle.addEventListener("click", () => {
    const next = document.documentElement.dataset.theme === "dark" ? "light" : "dark";
    storeTheme(next);
    setTheme(next);
  });
  systemDarkQuery.addEventListener("change", () => {
    if (!storedTheme()) setTheme(systemTheme());
  });
  setTheme(document.documentElement.dataset.theme === "dark" ? "dark" : "light");

  // --- Wiring ------------------------------------------------------------

  $("areas-controls").addEventListener("change", (event) => {
    const { name, value } = event.target;
    if (name in ALLOWED) update({ [name]: value, page: 1 });
  });
  $("areas-controls").addEventListener("submit", (event) => event.preventDefault());
  $("prev-page").addEventListener("click", () => update({ page: Math.max(1, current.page - 1) }));
  $("next-page").addEventListener("click", () => update({ page: current.page + 1 }));
  window.addEventListener("hashchange", () => {
    current = readHash();
    syncControls(current);
    loadAreas();
  });

  // Back to the map view the visitor came from: history.back() restores the
  // map's own `map=` hash. A visitor who arrived directly gets a plain link.
  $("back-link").addEventListener("click", (event) => {
    let sameOrigin = false;
    try {
      sameOrigin = document.referrer && new URL(document.referrer).origin === location.origin;
    } catch (e) {
      /* Unparseable referrer: treat as foreign. */
    }
    if (sameOrigin && history.length > 1) {
      event.preventDefault();
      history.back();
    }
  });

  syncControls(current);
  fetchJson("stats")
    .then(renderSummary)
    .catch((e) => {
      $("computed-at").textContent = "";
      showError(`Nie udało się wczytać statystyk: ${e.message}`);
    });
  loadAreas();
})();
