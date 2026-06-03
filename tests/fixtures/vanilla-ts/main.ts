interface Config {
  port: number;
  host: string;
}

const config: Config = { port: 3000, host: "localhost" };

enum Status {
  Active = "active",
  Inactive = "inactive",
}

class Server {
  constructor(public config: Config) {}
  get url(): string {
    return `http://${this.config.host}:${this.config.port}`;
  }
}

const server = new Server(config);
console.log(`status=${Status.Active}`);
console.log(`url=${server.url}`);
console.log("OK");
