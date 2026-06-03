class TempFile implements Disposable {
  name: string;
  constructor(name: string) {
    this.name = name;
  }
  [Symbol.dispose]() {
    console.log("close:" + this.name);
  }
}

class AsyncConn implements AsyncDisposable {
  id: string;
  constructor(id: string) {
    this.id = id;
  }
  async [Symbol.asyncDispose]() {
    console.log("disconnect:" + this.id);
  }
}

function syncBlock() {
  using f1 = new TempFile("a.txt");
  using f2 = new TempFile("b.txt");
  console.log("sync:" + f1.name + "," + f2.name);
}

async function asyncBlock() {
  await using c = new AsyncConn("db");
  console.log("async:" + c.id);
}

syncBlock();
await asyncBlock();
console.log("using:done");
