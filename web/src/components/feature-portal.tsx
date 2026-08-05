import type { ReactNode } from "react";

import { CubeMark } from "./cube-mark";

export type FeaturePortalPhase = "idle" | "closing" | "opening" | "open";

export type FeaturePortalOrigin = {
  column: number;
  row: number;
};

export function FeaturePortal({
  children,
  origin,
  phase,
}: {
  children: ReactNode;
  origin: FeaturePortalOrigin;
  phase: FeaturePortalPhase;
}) {
  const cubePaused = phase === "open";

  return (
    <div className={`feature-portal feature-portal--${phase}`}>
      <div className="feature-portal__core" aria-hidden="true">
        <CubeMark
          cameraDistance={21}
          className="feature-portal__cube"
          focusCell={origin}
          focused={phase === "open"}
          interactive={phase === "idle"}
          paused={cubePaused}
          returning={phase === "closing"}
          size={260}
          solving
          zooming={phase === "opening"}
        />
      </div>
      <div className="feature-portal__content" inert={phase === "open" ? undefined : true}>
        {children}
      </div>
    </div>
  );
}
