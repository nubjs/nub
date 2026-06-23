<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import benchmarkResults from "../../../benchmarks/results.json";

declare const __AUBE_VERSION__: string;
declare const __AUBE_RELEASED_AT__: string;

type BenchmarkRow = {
  key: string;
  values: Record<string, number>;
};

const chartTools = ["aube", "bun", "deno", "pnpm", "npm"] as const;
const chartLabels: Record<(typeof chartTools)[number], string> = {
  aube: "aube",
  bun: "bun",
  deno: "deno",
  pnpm: "pnpm",
  npm: "npm",
};
const warmInstallBenchmark = (benchmarkResults.rows as BenchmarkRow[]).find(
  (row) => row.key === "gvs-warm",
);
const warmInstallValues = warmInstallBenchmark?.values ?? {};
const maxWarmInstallValue = Math.max(
  ...chartTools.map((tool) => warmInstallValues[tool] ?? 0),
  1,
);
const chartRows = chartTools.map((tool) => {
  const value = warmInstallValues[tool] ?? 0;
  const width = ((value / maxWarmInstallValue) * 100).toFixed(2);

  return {
    tool,
    label: chartLabels[tool],
    style: { "--bar-width": `${width}%` },
  };
});
const benchmarkMultiple = (tool: "bun" | "pnpm") => {
  const aube = warmInstallValues.aube;
  const value = warmInstallValues[tool];
  if (!aube || !value) return "";
  return (value / aube).toFixed(1);
};
const pnpmWarmInstallMultiple = benchmarkMultiple("pnpm");
const bunWarmInstallMultiple = benchmarkMultiple("bun");

const packages = [
  "@vue/compiler-sfc@3.5.32",
  "vitepress@1.6.4",
  "react@18.3.1",
  "typescript@5.6.3",
  "vite@5.4.11",
  "esbuild@0.24.0",
  "rollup@4.28.0",
  "@types/node@22.10.1",
  "tailwindcss@3.4.15",
  "postcss@8.4.49",
  "zod@3.24.1",
  "hono@4.6.13",
  "vitest@2.1.8",
  "playwright@1.49.1",
  "astro@5.1.1",
  "three@0.171.0",
];

const spinners = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const progress = ref(0);
const spinnerIndex = ref(0);
const packageIndex = ref(0);
const done = ref(false);
const copiedInstallCommand = ref(false);
const releasePhrase = ref(__AUBE_RELEASED_AT__ ? "released recently" : "release date pending");
const progressBarEl = ref<HTMLElement | null>(null);
const progressBarColumns = ref(26);
const installedPackageTotal = 319;
let frame = 0;
let start = 0;
let releaseTimer = 0;
let progressResizeObserver: ResizeObserver | undefined;

const installed = computed(() => Math.floor(progress.value * installedPackageTotal));
const spinner = computed(() => spinners[spinnerIndex.value]);
const aubeVersion = __AUBE_VERSION__;
const releasedAt = __AUBE_RELEASED_AT__;
const terminalVersion = aubeVersion.split("-")[0] || aubeVersion;
const releaseNotesUrl = `https://github.com/jdx/aube/releases/tag/v${aubeVersion}`;
function formatReleasePhrase() {
  if (!releasedAt) {
    releasePhrase.value = "release date pending";
    return;
  }

  const date = new Date(releasedAt);
  if (Number.isNaN(date.getTime())) {
    releasePhrase.value = "release date pending";
    return;
  }

  const daysSinceRelease = Math.floor((Date.now() - date.getTime()) / 86_400_000);
  if (daysSinceRelease < 0) releasePhrase.value = "release pending";
  else if (daysSinceRelease === 0) releasePhrase.value = "released today";
  else if (daysSinceRelease === 1) releasePhrase.value = "released yesterday";
  else if (daysSinceRelease === 2) releasePhrase.value = "released 2 days ago";
  else if (daysSinceRelease < 7) releasePhrase.value = "released this week";
  else if (daysSinceRelease < 14) releasePhrase.value = "released last week";
  else if (daysSinceRelease < 31) releasePhrase.value = "released this month";
  else if (daysSinceRelease < 62) releasePhrase.value = "released last month";
  else if (daysSinceRelease < 365) releasePhrase.value = "released this year";
  else releasePhrase.value = "released a while ago";
}

const progressBar = computed(() => {
  const width = progressBarColumns.value;
  const filled = Math.floor(progress.value * width);
  return "█".repeat(filled) + "░".repeat(width - filled);
});
const packageRows = computed(() => [
  packages[packageIndex.value % packages.length],
  packages[(packageIndex.value + 7) % packages.length],
  packages[(packageIndex.value + 13) % packages.length],
]);

async function copyInstallCommand() {
  await navigator.clipboard?.writeText("mise use aube");
  copiedInstallCommand.value = true;
  window.setTimeout(() => {
    copiedInstallCommand.value = false;
  }, 1400);
}

function tick(now: number) {
  if (!start) start = now;
  const elapsed = now - start;
  progress.value = Math.min(1, elapsed / 2400);
  spinnerIndex.value = Math.floor(elapsed / 80) % spinners.length;
  packageIndex.value = Math.floor(elapsed / 110);

  if (progress.value < 1) {
    frame = requestAnimationFrame(tick);
    return;
  }

  done.value = true;
}

function updateProgressBarColumns() {
  const el = progressBarEl.value;
  if (!el) return;

  const styles = getComputedStyle(el);
  const probe = document.createElement("span");
  probe.textContent = "#".repeat(24);
  probe.style.fontFamily = styles.fontFamily;
  probe.style.fontSize = styles.fontSize;
  probe.style.fontWeight = styles.fontWeight;
  probe.style.letterSpacing = styles.letterSpacing;
  probe.style.position = "absolute";
  probe.style.visibility = "hidden";
  probe.style.whiteSpace = "pre";
  document.body.appendChild(probe);
  const charWidth = probe.getBoundingClientRect().width / 24;
  probe.remove();

  if (!charWidth) return;
  progressBarColumns.value = Math.max(8, Math.floor(el.getBoundingClientRect().width / charWidth));
}

onMounted(() => {
  formatReleasePhrase();
  releaseTimer = window.setInterval(formatReleasePhrase, 3_600_000);
  progressResizeObserver = new ResizeObserver(updateProgressBarColumns);
  if (progressBarEl.value) {
    progressResizeObserver.observe(progressBarEl.value);
    updateProgressBarColumns();
  }
  frame = requestAnimationFrame(tick);
});

onBeforeUnmount(() => {
  cancelAnimationFrame(frame);
  clearInterval(releaseTimer);
  progressResizeObserver?.disconnect();
});

watch(progressBarEl, (el, previousEl) => {
  if (previousEl) progressResizeObserver?.unobserve(previousEl);
  if (!el) return;
  progressResizeObserver?.observe(el);
  updateProgressBarColumns();
});
</script>

<template>
  <main class="aube-home">
    <div class="aube-hero-glow" aria-hidden="true"></div>
    <div class="aube-release">
      <span></span>
      <span>
        <a
          class="aube-release-version"
          :href="releaseNotesUrl"
          target="_blank"
          rel="noreferrer"
        >v{{ aubeVersion }}</a>
        · {{ releasePhrase }}
      </span>
    </div>
    <section class="aube-hero" aria-labelledby="aube-hero-title">
      <div class="aube-hero-copy">
        <p class="aube-pronunciation">aube /ob/ - pronounced "ohb"</p>
        <h1 id="aube-hero-title">Never forget <em>to install.</em></h1>
        <p class="aube-lede">
          Aube installs automatically when you run a script. The tightest
          security defaults of any Node.js package manager - and the only
          one with a lifecycle-script jail. Drops into existing projects
          using existing lockfiles.
        </p>
        <div class="aube-actions" aria-label="Primary links">
          <a class="aube-button aube-button-primary" href="/guide/">
            Start running
            <span aria-hidden="true">-></span>
          </a>
          <div class="aube-install-stack">
            <div class="aube-install-command" aria-label="Install command">
              <span class="aube-button-prompt">$</span>
              <code class="aube-install-code">mise use aube</code>
              <button type="button" @click="copyInstallCommand">
                {{ copiedInstallCommand ? "copied" : "copy" }}
              </button>
            </div>
            <a class="aube-install-link" href="/installation">Other install methods</a>
          </div>
        </div>
        <dl class="aube-stats">
          <div class="aube-stat-linked">
            <a
              class="aube-stat-link"
              href="/benchmarks"
              :aria-label="`See benchmarks — aube is ${pnpmWarmInstallMultiple}x faster than pnpm`"
            ></a>
            <dt>{{ pnpmWarmInstallMultiple }}x</dt>
            <dd>faster than pnpm</dd>
          </div>
          <div class="aube-stat-linked">
            <a
              class="aube-stat-link"
              href="/benchmarks"
              :aria-label="`See benchmarks — aube is ${bunWarmInstallMultiple}x faster than bun`"
            ></a>
            <dt>{{ bunWarmInstallMultiple }}x</dt>
            <dd>faster than bun</dd>
          </div>
          <div class="aube-stat-with-tip">
            <dt>
              90%
              <span class="aube-stat-tip">
                <button type="button" aria-describedby="disk-space-tip">?</button>
                <span id="disk-space-tip" role="tooltip">
                  npm copies dependencies into every project. Aube keeps package
                  files in one global store and links projects to it, so three
                  apps with React, Vite, TypeScript, and Playwright share the
                  heavy files instead of storing three full copies.
                </span>
              </span>
            </dt>
            <dd>
              less disk space than npm
            </dd>
          </div>
        </dl>
      </div>

      <div class="aube-stage" aria-label="Aube script run preview">
        <div class="aube-terminal" aria-label="Interactive script run output">
          <div class="aube-terminal-bar">
            <span></span>
            <span></span>
            <span></span>
            <strong>aubr test</strong>
          </div>
          <div class="aube-clx" aria-hidden="true">
            <div class="aube-command">$ mise use aube</div>
            <div class="aube-mise-output">mise aube@{{ aubeVersion }}   ✓ installed</div>
            <div class="aube-mise-output">mise ./mise.toml tools: aube@{{ aubeVersion }}</div>
            <div class="aube-command">$ aubr test</div>
            <template v-if="!done">
              <div class="aube-progress-root">
                <span class="aube-name">aube</span>
                <span class="aube-version">{{ terminalVersion }}</span>
                <span class="aube-byline">by jdx.dev</span>
                <span class="aube-phase">fetching</span>
                <span ref="progressBarEl" class="aube-progress-bar">{{ progressBar }}</span>
                <span class="aube-progress-count">{{ installed }}/{{ installedPackageTotal }} pkgs</span>
              </div>
              <div
                v-for="(pkg, index) in packageRows"
                :key="`${pkg}-${index}`"
                class="aube-fetch-row"
                :style="{ opacity: String(1 - index * 0.28) }"
              >
                <span class="aube-spinner">{{ index === 0 ? spinner : spinners[(spinnerIndex + index * 3) % spinners.length] }}</span>
                <span>{{ pkg }}</span>
              </div>
            </template>
            <template v-else>
              <div class="aube-install-summary">
                <span class="aube-name">aube</span>
                <span>{{ aubeVersion }}</span>
                <span class="aube-byline">by jdx.dev ·</span>
                <span class="aube-done-check">✓</span>
                <span>installed {{ installedPackageTotal }} packages in 3.7s</span>
              </div>
              <div class="aube-run-ok">✓ ran 100 tests successfully</div>
              <div class="aube-command">$ <span class="aube-caret">▍</span></div>
            </template>
          </div>
        </div>
      </div>
    </section>

    <section class="aube-proof" aria-label="Highlights">
      <a class="aube-proof-item" href="/benchmarks">
        <span class="aube-proof-number">01</span>
        <span class="aube-proof-tag">speed</span>
        <span class="aube-proof-visual aube-proof-chart" aria-hidden="true">
          <span
            v-for="row in chartRows"
            :key="row.tool"
            class="aube-chart-row"
            :class="`aube-chart-row-${row.tool}`"
            :style="row.style"
          >
            <span>{{ row.label }}</span>
            <span><i></i></span>
          </span>
        </span>
        <strong>Fastest Node.js package manager.</strong>
        <span>
          In the warm install benchmark, aube is
          {{ pnpmWarmInstallMultiple }}x faster than pnpm and
          {{ bunWarmInstallMultiple }}x faster than Bun. The chart shows warm
          installs with no <code>node_modules</code>; the other benchmarks
          cover CI and cold-cache cases.
        </span>
        <span class="aube-proof-link">See the benchmarks -></span>
      </a>
      <a class="aube-proof-item" href="/package-manager/lockfiles">
        <span class="aube-proof-number">02</span>
        <span class="aube-proof-tag">lockfiles</span>
        <span class="aube-proof-visual aube-proof-lockfiles" aria-hidden="true">
          <span class="aube-lockfile-formats">
            <code>yarn.lock</code>
            <code>pnpm-lock.yaml</code>
            <code>package-lock.json</code>
          </span>
          <span class="aube-lockfile-loop">
            <span>read</span>
            <i></i>
            <b>aube</b>
            <i></i>
            <span>write</span>
          </span>
          <span class="aube-lockfile-caption">same lockfile, written back</span>
        </span>
        <strong>Use existing lockfiles.</strong>
        <span>
          Read and write <code>yarn.lock</code>, <code>pnpm-lock.yaml</code>,
          or <code>package-lock.json</code> in place without forcing a team-wide migration.
        </span>
        <span class="aube-proof-link">Lockfile compatibility -></span>
      </a>
      <a class="aube-proof-item" href="/package-manager/scripts">
        <span class="aube-proof-number">03</span>
        <span class="aube-proof-tag">repeat</span>
        <span class="aube-proof-visual aube-proof-run" aria-hidden="true">
          <span><b>$</b> aubr test</span>
          <span class="aube-run-install">deps stale · install first</span>
          <span class="aube-run-ok">✓ ran 100 tests successfully</span>
          <span><b>$</b> aubr test</span>
          <span class="aube-run-ok">deps fresh · ran 100 tests successfully</span>
        </span>
        <strong>Run scripts instead of installing.</strong>
        <span><code>aubr test</code> auto-installs first when dependencies changed, then skips that work on repeat runs. Use <code>aubx</code> for one-off tools.</span>
        <span class="aube-proof-link">Run scripts and binaries -></span>
      </a>
      <a class="aube-proof-item" href="/package-manager/node-modules">
        <span class="aube-proof-number">04</span>
        <span class="aube-proof-tag">disk</span>
        <span class="aube-proof-visual aube-proof-disk" aria-hidden="true">
          <span class="aube-disk-packages">
            <i>react@19.1</i>
            <i>vite@7.1</i>
            <i>typescript@5.9</i>
            <i>react@19.1</i>
            <i>vite@7.1</i>
            <i>typescript@5.9</i>
            <i>react@19.1</i>
            <i>vite@7.1</i>
            <i>typescript@5.9</i>
          </span>
          <span class="aube-disk-funnel"></span>
          <span class="aube-disk-store">aube store</span>
          <span class="aube-disk-projects">
            <b>app</b>
            <b>api</b>
            <b>web</b>
          </span>
        </span>
        <strong>Use less disk.</strong>
        <span>
          A global content-addressable store lets projects share package files
          instead of keeping a full copy in every checkout.
        </span>
        <span class="aube-proof-link">node_modules layout -></span>
      </a>
      <a class="aube-proof-item" href="/security">
        <span class="aube-proof-number">05</span>
        <span class="aube-proof-tag">secure</span>
        <span class="aube-proof-visual aube-proof-scripts" aria-hidden="true">
          <span class="aube-script-row aube-script-row-root">
            <b>trust downgrades</b>
            <i>blocked</i>
          </span>
          <span class="aube-script-row">
            <b>new releases</b>
            <i>24h cooling</i>
          </span>
          <span class="aube-script-row">
            <b>typosquats</b>
            <i>checked on add</i>
          </span>
          <span class="aube-script-row">
            <b>build scripts</b>
            <i>deny by default</i>
          </span>
        </span>
        <strong>Supply-chain defaults across the install path.</strong>
        <span>
          Trust downgrades fail at resolve, new releases sit out a 24h cooling
          window, <code>aube add</code> blocks known-malicious packages and
          prompts on near-zero-download installs, lifecycle scripts wait for
          approval, and exotic transitive deps are blocked.
          <code>paranoid: true</code> adds the build jail and turns the soft
          gates into hard fails.
        </span>
        <span class="aube-proof-link">Security overview -></span>
      </a>
    </section>
  </main>
</template>
