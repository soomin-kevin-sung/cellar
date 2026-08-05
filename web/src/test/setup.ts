import "@testing-library/jest-dom/vitest";
import { createElement } from "react";

vi.mock("@react-three/fiber", () => ({
  Canvas: ({ className }: { className?: string }) => createElement("canvas", { className }),
  useFrame: vi.fn(),
}));
