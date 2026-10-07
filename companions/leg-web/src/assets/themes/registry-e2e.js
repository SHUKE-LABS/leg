export const themes = [
  {
    id: "default",
    name: "Default",
    stylesheet: "/themes/default.css",
    load: () => import("/themes/default.js"),
  },
  {
    id: "fixture",
    name: "Fixture",
    stylesheet: "/themes/fixture.css",
    load: () => import("/themes/fixture.js"),
  },
];
