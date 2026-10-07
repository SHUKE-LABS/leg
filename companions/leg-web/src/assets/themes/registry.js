export const themes = [
  {
    id: "default",
    name: "Default",
    stylesheet: "/themes/default.css",
    load: () => import("/themes/default.js"),
  },
];
