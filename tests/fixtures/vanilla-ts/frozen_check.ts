console.log(
  "extensible:" +
    Object.isExtensible(Object.prototype) +
    "," +
    Object.isExtensible(Array.prototype) +
    "," +
    Object.isExtensible(String.prototype)
);
