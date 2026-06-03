const pattern = new URLPattern({ pathname: "/books/:id" });
const match = pattern.exec("https://example.com/books/42");
console.log("urlpattern-id:" + match?.pathname.groups.id);
console.log("urlpattern-nomatch:" + (pattern.exec("https://example.com/movies/1") === null));
