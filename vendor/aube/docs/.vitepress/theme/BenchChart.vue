<script setup lang="ts">
import { computed } from "vue";

interface Row {
  label: string;
  values: Record<string, number | null>;
}

const props = defineProps<{
  rows: Row[];
  managers: string[];
  unit?: string;
  versions?: Record<string, string>;
}>();

const COLORS: Record<string, string> = {
  npm: "#cb3837",
  yarn: "#2c8ebb",
  pnpm: "#f9ad00",
  // Bun's brand cream (#fbf0df) is illegible on a light VitePress
  // background, so use a darker tan that stays on-brand but is
  // actually readable.
  bun: "#c9a36a",
  // Deno's wordmark is black on light backgrounds; the seafoam green
  // from the brand kit reads better next to the other tools' chips.
  deno: "#70c7a9",
  vlt: "#5b4eef",
  aube: "#7c3aed",
};

function legendLabel(pm: string): string {
  const v = props.versions?.[pm];
  return v ? `${pm} ${v}` : pm;
}

const nodeVersion = computed(() => props.versions?.node ?? "");

function rowMax(row: Row): number {
  let m = 0;
  for (const pm of props.managers) {
    const v = row.values[pm];
    if (v != null && v > m) m = v;
  }
  return m || 1;
}

function format(ms: number): string {
  if (ms >= 1000) return `${(ms / 1000).toFixed(2)}s`;
  return `${Math.round(ms)}ms`;
}

function winner(row: Row): string | null {
  let best: string | null = null;
  let bestVal = Infinity;
  for (const pm of props.managers) {
    const v = row.values[pm];
    if (v != null && v < bestVal) {
      bestVal = v;
      best = pm;
    }
  }
  return best;
}
</script>

<template>
  <div class="bench-chart">
    <div class="legend">
      <span v-for="pm in managers" :key="pm" class="legend-item">
        <span class="swatch" :style="{ background: COLORS[pm] || '#888' }"></span>
        {{ legendLabel(pm) }}
      </span>
      <span v-if="nodeVersion" class="legend-runtime">node {{ nodeVersion }}</span>
    </div>
    <div v-for="row in rows" :key="row.label" class="scenario">
      <div class="scenario-label">{{ row.label }}</div>
      <div class="bars">
        <template v-for="pm in managers" :key="pm">
          <div class="bar-row">
            <div class="bar-name">{{ pm }}</div>
            <div class="bar-track">
              <div
                v-if="row.values[pm] != null"
                class="bar"
                :class="{ winner: winner(row) === pm }"
                :style="{
                  width: ((row.values[pm]! / rowMax(row)) * 100) + '%',
                  background: COLORS[pm] || '#888',
                }"
              ></div>
              <div v-else class="bar-missing">—</div>
            </div>
            <div class="bar-value">
              {{ row.values[pm] != null ? format(row.values[pm]!) : "" }}
            </div>
          </div>
        </template>
      </div>
    </div>
  </div>
</template>

<style scoped>
.bench-chart {
  margin: 1.5rem 0;
  font-size: 14px;
}
.legend {
  display: flex;
  gap: 1rem;
  flex-wrap: wrap;
  margin-bottom: 1rem;
  padding-bottom: 0.75rem;
  border-bottom: 1px solid var(--vp-c-divider);
}
.legend-item {
  display: inline-flex;
  align-items: center;
  gap: 0.4rem;
  color: var(--vp-c-text-2);
}
.legend-runtime {
  color: var(--vp-c-text-3);
  font-variant-numeric: tabular-nums;
  margin-left: auto;
}
.swatch {
  display: inline-block;
  width: 12px;
  height: 12px;
  border-radius: 2px;
}
.scenario {
  margin-bottom: 1.25rem;
}
.scenario-label {
  font-weight: 600;
  margin-bottom: 0.4rem;
  color: var(--vp-c-text-1);
}
.bars {
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.bar-row {
  display: grid;
  grid-template-columns: 52px 1fr 64px;
  align-items: center;
  gap: 0.5rem;
}
.bar-name {
  color: var(--vp-c-text-2);
  font-variant-numeric: tabular-nums;
}
.bar-track {
  position: relative;
  height: 18px;
  background: var(--vp-c-bg-soft);
  border-radius: 3px;
  overflow: hidden;
}
.bar {
  height: 100%;
  border-radius: 3px;
  transition: width 0.3s ease;
  min-width: 2px;
}
.bar.winner {
  box-shadow: 0 0 0 1px var(--vp-c-text-1) inset;
}
.bar-missing {
  padding-left: 6px;
  color: var(--vp-c-text-3);
  line-height: 18px;
}
.bar-value {
  text-align: right;
  font-variant-numeric: tabular-nums;
  color: var(--vp-c-text-1);
}
</style>
