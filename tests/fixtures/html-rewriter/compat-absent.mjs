// Under --node / NODE_COMPAT (zero-augmentation mode) the HTMLRewriter global must
// NOT be installed — augmentation is off and no Node version ships it natively.
console.log("HTMLREWRITER:", typeof HTMLRewriter);
