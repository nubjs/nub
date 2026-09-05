export class Token {}
export let value = 41;
export function increment() { value++; }
export let disposals = 0;
{
  using resource = {[Symbol.dispose]() { disposals++; }};
}
