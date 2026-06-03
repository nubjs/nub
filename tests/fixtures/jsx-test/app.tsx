const App = ({ name }: { name: string }) => <div>Hello, {name}!</div>;
console.log(JSON.stringify(App({ name: "JSX" })));
