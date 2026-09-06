import { readFile, writeFile } from 'node:fs/promises'

// usage-lib does not model relationships between flags, so correct generated
// usage strings whose optionality depends on another flag.
const replacements = [
	{
		path: 'docs/cli/index.md',
		from: 'aube store prune [--dry-run] [--json]',
		to: 'aube store prune [--dry-run [--json]]',
	},
	{
		path: 'docs/cli/store.md',
		from: 'aube store prune [--dry-run] [--json]',
		to: 'aube store prune [--dry-run [--json]]',
	},
	{
		path: 'docs/cli/store/prune.md',
		from: 'aube store prune [--dry-run] [--json]',
		to: 'aube store prune [--dry-run [--json]]',
	},
	{
		path: 'docs/cli/commands.json',
		from: '"usage": "store prune [--dry-run] [--json]"',
		to: '"usage": "store prune [--dry-run [--json]]"',
	},
]

for (const { path, from, to } of replacements) {
	const source = await readFile(path, 'utf8')
	const occurrences = source.split(from).length - 1
	if (occurrences !== 1) {
		throw new Error(`expected exactly one generated usage string in ${path}, found ${occurrences}`)
	}
	await writeFile(path, source.replace(from, to))
}
