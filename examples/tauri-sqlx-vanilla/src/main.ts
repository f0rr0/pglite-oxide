import { invoke } from "@tauri-apps/api/core";

type PhaseTiming = {
  name: string;
  ms: number;
};

type QueryTiming = {
  label: string;
  iterations: number;
  minMs: number;
  p50Ms: number;
  p95Ms: number;
  maxMs: number;
  meanMs: number;
  rows: number;
};

type BenchReport = {
  root: string;
  proxyAddr: string;
  coldStart: boolean;
  pgdataTemplate: boolean;
  rowCount: number;
  startup: PhaseTiming[];
  workload: PhaseTiming[];
  queries: QueryTiming[];
  totalMs: number;
  notes: string[];
};

const statusEl = document.querySelector<HTMLDivElement>("#status");
const formEl = document.querySelector<HTMLFormElement>("#profile-form");
const rowCountEl = document.querySelector<HTMLInputElement>("#row-count");
const freshRunEl = document.querySelector<HTMLInputElement>("#fresh-run");
const runButtonEl = document.querySelector<HTMLButtonElement>("#run-profile");
const startupTotalEl = document.querySelector<HTMLElement>("#startup-total");
const workloadTotalEl = document.querySelector<HTMLElement>("#workload-total");
const profileRowsEl = document.querySelector<HTMLElement>("#profile-rows");
const proxyAddrEl = document.querySelector<HTMLElement>("#proxy-addr");
const startupModeEl = document.querySelector<HTMLElement>("#startup-mode");
const profileRootEl = document.querySelector<HTMLElement>("#profile-root");
const startupListEl = document.querySelector<HTMLElement>("#startup-list");
const workloadListEl = document.querySelector<HTMLElement>("#workload-list");
const queryTableEl = document.querySelector<HTMLTableSectionElement>("#query-table");
const notesEl = document.querySelector<HTMLElement>("#notes");

function sumMs(phases: PhaseTiming[]) {
  return phases.reduce((total, phase) => total + phase.ms, 0);
}

function formatMs(ms: number) {
  if (!Number.isFinite(ms)) return "-";
  if (ms >= 1000) return `${(ms / 1000).toFixed(2)} s`;
  return `${ms.toFixed(ms >= 10 ? 1 : 2)} ms`;
}

function formatCount(value: number) {
  return new Intl.NumberFormat().format(value);
}

function renderPhases(container: HTMLElement | null, phases: PhaseTiming[]) {
  if (!container) return;
  const max = Math.max(...phases.map((phase) => phase.ms), 1);
  container.replaceChildren(
    ...phases.map((phase) => {
      const row = document.createElement("div");
      row.className = "phase";

      const label = document.createElement("span");
      label.textContent = phase.name;

      const bar = document.createElement("i");
      bar.style.inlineSize = `${Math.max((phase.ms / max) * 100, 2)}%`;

      const value = document.createElement("strong");
      value.textContent = formatMs(phase.ms);

      row.append(label, bar, value);
      return row;
    }),
  );
}

function renderQueries(queries: QueryTiming[]) {
  if (!queryTableEl) return;
  queryTableEl.replaceChildren(
    ...queries.map((query) => {
      const row = document.createElement("tr");
      const cells = [
        query.label,
        formatCount(query.iterations),
        formatCount(query.rows),
        formatMs(query.meanMs),
        formatMs(query.p50Ms),
        formatMs(query.p95Ms),
        formatMs(query.maxMs),
      ];

      for (const cell of cells) {
        const td = document.createElement("td");
        td.textContent = cell;
        row.append(td);
      }
      return row;
    }),
  );
}

function renderNotes(notes: string[]) {
  if (!notesEl) return;
  notesEl.replaceChildren(
    ...notes.map((note) => {
      const item = document.createElement("p");
      item.textContent = note;
      return item;
    }),
  );
}

function renderReport(report: BenchReport) {
  if (startupTotalEl) startupTotalEl.textContent = formatMs(sumMs(report.startup));
  if (workloadTotalEl) workloadTotalEl.textContent = formatMs(report.totalMs);
  if (profileRowsEl) profileRowsEl.textContent = formatCount(report.rowCount);
  if (proxyAddrEl) proxyAddrEl.textContent = report.proxyAddr;
  if (startupModeEl) {
    startupModeEl.textContent = report.coldStart
      ? report.pgdataTemplate
        ? "cold template"
        : "cold initdb"
      : "warm reuse";
  }
  if (profileRootEl) profileRootEl.textContent = report.root;

  renderPhases(startupListEl, report.startup);
  renderPhases(workloadListEl, report.workload);
  renderQueries(report.queries);
  renderNotes(report.notes);
}

async function runProfile() {
  const rowCount = Number(rowCountEl?.value || 10_000);
  const fresh = Boolean(freshRunEl?.checked);

  if (runButtonEl) runButtonEl.disabled = true;
  if (statusEl) statusEl.textContent = "Running";

  try {
    const report = await invoke<BenchReport>("profile_queries", {
      fresh,
      rowCount,
    });
    renderReport(report);
    if (freshRunEl) freshRunEl.checked = false;
    if (statusEl) statusEl.textContent = "Complete";
  } catch (error) {
    if (statusEl) statusEl.textContent = "Failed";
    renderNotes([String(error)]);
  } finally {
    if (runButtonEl) runButtonEl.disabled = false;
  }
}

formEl?.addEventListener("submit", (event) => {
  event.preventDefault();
  runProfile();
});
