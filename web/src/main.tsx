import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { CellarRoot } from "./components/auth-root";
import "./styles.css";

const root = document.getElementById("root");

if (!root) throw new Error("Application root element is missing.");

createRoot(root).render(
  <StrictMode>
    <CellarRoot />
    {import.meta.env.DEV ? <span aria-label="개발 환경" className="dev-indicator">DEV</span> : null}
  </StrictMode>,
);
