enum Tag { Child = "forked-child" }
process.on("message", (msg: any) => {
  process.send!({ echo: msg.value, tag: Tag.Child });
});
