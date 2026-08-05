import { render, screen } from "@testing-library/react";

import { FeaturePortal } from "./feature-portal";

describe("FeaturePortal", () => {
  it("keeps the three-face cube active while the feature surface is idle", () => {
    render(<FeaturePortal origin={{ column: 2, row: 2 }} phase="idle"><button type="button">Feature</button></FeaturePortal>);

    expect(document.querySelector(".cube-mark--solving")).toBeInTheDocument();
    expect(document.querySelector(".cube-mark__canvas")).toBeInTheDocument();
    expect(document.querySelector(".cube-mark__drag-surface")).toBeInTheDocument();
    expect(document.querySelector(".cube-mark--paused")).not.toBeInTheDocument();
    expect(document.querySelector(".feature-portal__content")).toHaveAttribute("inert");
  });

  it("starts zooming immediately while keeping the feature inert", () => {
    render(<FeaturePortal origin={{ column: 4, row: 0 }} phase="opening"><div>Selected feature</div></FeaturePortal>);

    expect(document.querySelector(".cube-mark--paused")).not.toBeInTheDocument();
    expect(document.querySelector(".cube-mark__drag-surface")).not.toBeInTheDocument();
    expect(document.querySelector(".feature-portal__content")).toHaveAttribute("inert");
    expect(screen.getByText("Selected feature")).toBeInTheDocument();
  });

  it("keeps the cube frozen after the feature is open", () => {
    render(<FeaturePortal origin={{ column: 2, row: 2 }} phase="open"><div>Open feature</div></FeaturePortal>);

    expect(document.querySelector(".cube-mark--paused")).toBeInTheDocument();
  });

  it("keeps the cube active while returning from a feature", () => {
    render(<FeaturePortal origin={{ column: 1, row: 3 }} phase="closing"><div>Closing feature</div></FeaturePortal>);

    expect(document.querySelector(".cube-mark--paused")).not.toBeInTheDocument();
  });
});
