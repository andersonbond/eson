import App from "./App.svelte";
import "./app.css";

(() => {
  const t = localStorage.getItem("eson_theme");
  document.documentElement.dataset.theme = t === "light" ? "light" : "dark";
})();

const target = document.getElementById("app");
if (target) {
  new App({ target });
}

export default App;
