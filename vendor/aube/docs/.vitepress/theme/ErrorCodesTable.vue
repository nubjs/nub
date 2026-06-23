<script setup lang="ts">
// Renders the `errors` or `warnings` slice of the registry produced
// by `cargo run -p aube-codes --bin generate-error-codes-docs`. The
// caller passes the rows + the category list (already in
// first-seen order) and toggles whether to show the Exit column.
//
// Search filters by code name + description (case-insensitive).
// State is purely local to the component — no router updates, no
// query params, no localStorage. Two of these mount on the same
// page (one for errors, one for warnings) so cross-component state
// would tangle them; keeping each independent is the simpler
// contract.

import { computed, ref } from "vue";

interface CodeMeta {
  name: string;
  category: string;
  description: string;
  exit_code: number | null;
}

const props = defineProps<{
  codes: CodeMeta[];
  categories: string[];
  /**
   * When true, render an Exit column. Errors set this; warnings
   * never do because warnings don't change exit status.
   */
  showExit: boolean;
}>();

const search = ref("");

const filtered = computed<CodeMeta[]>(() => {
  const needle = search.value.trim().toLowerCase();
  if (!needle) {
    return props.codes;
  }
  return props.codes.filter(
    (c) =>
      c.name.toLowerCase().includes(needle) ||
      c.description.toLowerCase().includes(needle),
  );
});

// Group filtered results by category, preserving the first-seen
// order in `props.categories`. Categories that drop to zero rows
// after filtering are skipped so the page doesn't render empty
// sub-headings.
const grouped = computed<Array<{ category: string; rows: CodeMeta[] }>>(
  () => {
    const buckets = new Map<string, CodeMeta[]>();
    for (const row of filtered.value) {
      const list = buckets.get(row.category) ?? [];
      list.push(row);
      buckets.set(row.category, list);
    }
    return props.categories
      .map((category) => ({
        category,
        rows: buckets.get(category) ?? [],
      }))
      .filter((g) => g.rows.length > 0);
  },
);

function exitLabel(code: CodeMeta): string {
  // Errors with no bespoke entry fall through to `EXIT_GENERIC = 1`
  // in `aube-codes::exit`. Render `1` here so the column always
  // carries a value — the alternative ("—" or empty) implies "no
  // exit code", which is wrong; every error exits non-zero.
  return code.exit_code === null ? "1" : String(code.exit_code);
}
</script>

<template>
  <div class="error-codes-table">
    <div class="error-codes-table__controls">
      <input
        v-model="search"
        type="search"
        placeholder="Filter by code or description…"
        class="error-codes-table__search"
        :aria-label="
          showExit
            ? 'Filter error codes by code or description'
            : 'Filter warning codes by code or description'
        "
      />
    </div>

    <p
      v-if="filtered.length === 0"
      class="error-codes-table__empty"
      role="status"
    >
      No codes match the current filter.
    </p>

    <template v-else>
      <section
        v-for="group in grouped"
        :key="group.category"
        class="error-codes-table__group"
      >
        <h3 :id="slugify(group.category)" class="error-codes-table__heading">
          {{ group.category }}
        </h3>
        <table class="error-codes-table__table">
          <thead>
            <tr>
              <th scope="col">Code</th>
              <th v-if="showExit" scope="col" class="error-codes-table__exit-col">
                Exit
              </th>
              <th scope="col">Description</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="row in group.rows" :key="row.name">
              <td>
                <code :id="row.name">{{ row.name }}</code>
              </td>
              <td v-if="showExit" class="error-codes-table__exit-cell">
                <code>{{ exitLabel(row) }}</code>
              </td>
              <td v-html="renderDescription(row.description)" />
            </tr>
          </tbody>
        </table>
      </section>
    </template>
  </div>
</template>

<script lang="ts">
function slugify(s: string): string {
  return s
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");
}

/**
 * The descriptions in the registry use Markdown backticks for inline
 * code. The Vue compiler can't render Markdown, but the convention
 * is narrow enough (only inline backticks appear) that a one-pass
 * substitution into `<code>` tags is correct, escaping anything
 * that isn't already a backtick. Anything outside the backticks is
 * HTML-escaped to prevent injection from a future description that
 * might include `<` or `&`.
 */
function renderDescription(text: string): string {
  let out = "";
  let i = 0;
  while (i < text.length) {
    const tick = text.indexOf("`", i);
    if (tick < 0) {
      out += escapeHtml(text.slice(i));
      break;
    }
    out += escapeHtml(text.slice(i, tick));
    const close = text.indexOf("`", tick + 1);
    if (close < 0) {
      // Unbalanced backtick — render the rest as plain text rather
      // than swallowing it.
      out += escapeHtml(text.slice(tick));
      break;
    }
    out += `<code>${escapeHtml(text.slice(tick + 1, close))}</code>`;
    i = close + 1;
  }
  return out;
}

function escapeHtml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}
</script>

<style scoped>
.error-codes-table {
  margin: 1.5rem 0;
}

.error-codes-table__controls {
  display: flex;
  flex-direction: column;
  gap: 0.75rem;
  margin-bottom: 1.5rem;
  position: sticky;
  /* Stick just below the fixed banner+navbar overlay. The banner
     contributes `--vp-layout-top-height` (0 when not rendered);
     `--vp-nav-height` is the navbar itself. Using only
     `--vp-nav-height` would tuck the top ~36px of the search input
     behind the navbar whenever the jdx-banner is active — same
     class of bug the layout's `padding-bottom` fix in banner.css
     addresses for the page-bottom side. */
  top: calc(var(--vp-layout-top-height, 0px) + var(--vp-nav-height, 64px));
  background: var(--vp-c-bg);
  padding: 0.75rem 0;
  z-index: 1;
}

.error-codes-table__search {
  width: 100%;
  padding: 0.5rem 0.75rem;
  border: 1px solid var(--vp-c-divider);
  border-radius: 6px;
  background: var(--vp-c-bg-soft);
  color: var(--vp-c-text-1);
  font-size: 0.95rem;
  font-family: inherit;
  transition: border-color 0.15s ease;
}

.error-codes-table__search:focus {
  outline: none;
  border-color: var(--vp-c-brand-1);
}

.error-codes-table__group {
  margin: 1.5rem 0;
}

.error-codes-table__heading {
  font-size: 1.1rem;
  margin: 1.25rem 0 0.5rem;
  /* Anchor jumps need to clear the same banner+navbar overlay the
     sticky `top:` above accounts for. Without the
     `--vp-layout-top-height` term, jumping to a category heading
     when the jdx-banner is active scrolls the heading text behind
     the banner — same offset bug, this side. The trailing `1rem`
     leaves a small visual gap between the navbar's bottom edge and
     the heading. */
  scroll-margin-top: calc(
    var(--vp-layout-top-height, 0px) + var(--vp-nav-height, 64px) + 1rem
  );
}

.error-codes-table__table {
  width: 100%;
  border-collapse: collapse;
  font-size: 0.9rem;
}

.error-codes-table__table th,
.error-codes-table__table td {
  text-align: left;
  vertical-align: top;
  padding: 0.5rem 0.75rem;
  border-bottom: 1px solid var(--vp-c-divider);
}

.error-codes-table__table th {
  font-weight: 600;
  color: var(--vp-c-text-2);
  background: var(--vp-c-bg-soft);
  white-space: nowrap;
}

.error-codes-table__exit-col,
.error-codes-table__exit-cell {
  width: 4rem;
  text-align: center;
  white-space: nowrap;
}

.error-codes-table__empty {
  margin: 1.5rem 0;
  color: var(--vp-c-text-2);
  font-style: italic;
}
</style>
