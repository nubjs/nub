export function verdict(jailed, control) {
  if (control?.error) return 'CONTROL-FAILED';
  if (jailed.error) return control ? 'JAIL-FAILED' : 'UNRESOLVED';
  return 'PASS';
}
