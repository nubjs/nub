interface Greeting {
  who: string;
}
const g: Greeting = { who: "sandbox" };
console.log(`TS-OK ${g.who}`);
