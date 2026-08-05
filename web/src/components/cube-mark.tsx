import { Canvas, useFrame, useThree } from "@react-three/fiber";
import {
  useEffect,
  useRef,
  useState,
  type CSSProperties,
  type MutableRefObject,
  type PointerEvent as ReactPointerEvent,
} from "react";
import * as THREE from "three";

type CubeMarkProps = {
  cameraDistance?: number;
  className?: string;
  focusCell?: { column: number; row: number };
  focused?: boolean;
  interactive?: boolean;
  onRevealComplete?: () => void;
  paused?: boolean;
  revealing?: boolean;
  returning?: boolean;
  size?: number;
  solving?: boolean;
  zooming?: boolean;
};

type Axis = "x" | "y" | "z";

type CubeModel = {
  cubeGroup: THREE.Group;
  cubies: THREE.Group[];
  edgeGeometry: THREE.EdgesGeometry;
  edgeMaterial: THREE.LineBasicMaterial;
  geometry: THREE.BoxGeometry;
  root: THREE.Group;
  surfaceMaterial: THREE.MeshBasicMaterial;
  turnGroup: THREE.Group;
};

type ActiveTurn = {
  axis: Axis;
  direction: -1 | 1;
};

type AnimationState = {
  activeTurn: ActiveTurn | null;
  alignFrom: { x: number; y: number; z: number };
  completionSent: boolean;
  elapsed: number;
  lastAxis: Axis | null;
  phase: "orbit" | "align" | "gap" | "turn" | "hold";
  turns: number;
};

type ZoomState = {
  elapsed: number;
  from: THREE.Vector3;
  rootFrom: { x: number; y: number; z: number };
};

type ReturnState = {
  elapsed: number;
  from: THREE.Vector3;
  rootFrom: { x: number; y: number; z: number };
};

type InteractionState = {
  base: { x: number; y: number; z: number };
  dragging: boolean;
  lastX: number;
  lastY: number;
  pointer: THREE.Vector2;
};

const OUTER_COORDINATE = 2;
const CUBIE_SPACING = 0.94;
const CUBIE_SIZE = 0.94;
const ORBIT_DURATION = 2;
const TURN_DURATION = 0.68;
const TURN_GAP = 0.24;
const TURNS_PER_SEQUENCE = 3;
const REVEAL_ALIGN_DURATION = 0.56;
const REVEAL_TURN_DURATION = 0.62;
const REVEAL_TURN_GAP = 0.16;
const FEATURE_ZOOM_DURATION = 0.9;
const FEATURE_RETURN_DURATION = 0.56;
const AXES: Axis[] = ["x", "y", "z"];

function createCubeModel(): CubeModel {
  const root = new THREE.Group();
  const cubeGroup = new THREE.Group();
  const turnGroup = new THREE.Group();
  const geometry = new THREE.BoxGeometry(CUBIE_SIZE, CUBIE_SIZE, CUBIE_SIZE);
  const edgeGeometry = new THREE.EdgesGeometry(geometry);
  const surfaceMaterial = new THREE.MeshBasicMaterial({ color: "#06100d" });
  const edgeMaterial = new THREE.LineBasicMaterial({
    color: "#668fb9",
    opacity: 0.78,
    transparent: true,
  });
  const cubies: THREE.Group[] = [];

  root.rotation.set(0.34, -0.55, 0);
  root.add(cubeGroup, turnGroup);

  for (let x = -OUTER_COORDINATE; x <= OUTER_COORDINATE; x += 1) {
    for (let y = -OUTER_COORDINATE; y <= OUTER_COORDINATE; y += 1) {
      for (let z = -OUTER_COORDINATE; z <= OUTER_COORDINATE; z += 1) {
        if (Math.max(Math.abs(x), Math.abs(y), Math.abs(z)) !== OUTER_COORDINATE) continue;

        const cubie = new THREE.Group();
        const body = new THREE.Mesh(geometry, surfaceMaterial);
        const edges = new THREE.LineSegments(edgeGeometry, edgeMaterial);
        cubie.position.set(x * CUBIE_SPACING, y * CUBIE_SPACING, z * CUBIE_SPACING);
        cubie.add(body, edges);
        cubeGroup.add(cubie);
        cubies.push(cubie);
      }
    }
  }

  return { cubeGroup, cubies, edgeGeometry, edgeMaterial, geometry, root, surfaceMaterial, turnGroup };
}

function easeInOutCubic(value: number) {
  return value < 0.5 ? 4 * value * value * value : 1 - Math.pow(-2 * value + 2, 3) / 2;
}

function normalizeCubie(cubie: THREE.Object3D) {
  cubie.position.set(
    Math.round(cubie.position.x / CUBIE_SPACING) * CUBIE_SPACING,
    Math.round(cubie.position.y / CUBIE_SPACING) * CUBIE_SPACING,
    Math.round(cubie.position.z / CUBIE_SPACING) * CUBIE_SPACING,
  );
  cubie.quaternion.identity();
  cubie.scale.set(1, 1, 1);
}

function beginRandomTurn(model: CubeModel, animation: AnimationState) {
  const candidates = AXES.filter((axis) => axis !== animation.lastAxis);
  const axis = candidates[Math.floor(Math.random() * candidates.length)] ?? "z";
  const layer = Math.floor(Math.random() * (OUTER_COORDINATE * 2 + 1)) - OUTER_COORDINATE;
  const direction: -1 | 1 = Math.random() > 0.5 ? 1 : -1;

  model.root.updateMatrixWorld(true);
  model.turnGroup.rotation.set(0, 0, 0);
  model.turnGroup.updateMatrixWorld(true);

  model.cubies
    .filter((cubie) => Math.round(cubie.position[axis] / CUBIE_SPACING) === layer)
    .forEach((cubie) => model.turnGroup.attach(cubie));

  animation.activeTurn = { axis, direction };
  animation.elapsed = 0;
  animation.lastAxis = axis;
  animation.phase = "turn";
}

function finishTurn(model: CubeModel, animation: AnimationState) {
  model.root.updateMatrixWorld(true);
  [...model.turnGroup.children].forEach((cubie) => {
    model.cubeGroup.attach(cubie);
    normalizeCubie(cubie);
  });
  model.turnGroup.rotation.set(0, 0, 0);
  animation.activeTurn = null;
  animation.elapsed = 0;
  animation.turns += 1;
  animation.phase = "gap";
}

function PuzzleCube({
  cameraDistance,
  focusCell,
  focused,
  interactionRef,
  onRevealComplete,
  paused,
  reducedMotion,
  revealing,
  returning,
  solving,
  zooming,
}: {
  cameraDistance: number;
  focusCell: { column: number; row: number };
  focused: boolean;
  interactionRef: MutableRefObject<InteractionState>;
  onRevealComplete?: () => void;
  paused: boolean;
  reducedMotion: boolean;
  revealing: boolean;
  returning: boolean;
  solving: boolean;
  zooming: boolean;
}) {
  const { camera } = useThree();
  const [model] = useState(createCubeModel);
  const modelRef = useRef(model);
  const onRevealCompleteRef = useRef(onRevealComplete);
  const animationRef = useRef<AnimationState>({
    activeTurn: null,
    alignFrom: { x: 0.34, y: -0.55, z: 0 },
    completionSent: false,
    elapsed: 0,
    lastAxis: null,
    phase: "orbit",
    turns: 0,
  });
  const returnRef = useRef<ReturnState>({
    elapsed: 0,
    from: new THREE.Vector3(0, 0, cameraDistance),
    rootFrom: { x: 0, y: 0, z: 0 },
  });
  const zoomRef = useRef<ZoomState>({
    elapsed: 0,
    from: new THREE.Vector3(0, 0, cameraDistance),
    rootFrom: { x: 0.34, y: -0.55, z: 0 },
  });

  useEffect(() => {
    onRevealCompleteRef.current = onRevealComplete;
  }, [onRevealComplete]);

  useEffect(() => {
    const updatePointer = (event: PointerEvent) => {
      interactionRef.current.pointer.set(
        (event.clientX / window.innerWidth) * 2 - 1,
        (event.clientY / window.innerHeight) * 2 - 1,
      );
    };
    const resetPointer = () => interactionRef.current.pointer.set(0, 0);
    window.addEventListener("pointermove", updatePointer, { passive: true });
    window.addEventListener("blur", resetPointer);
    return () => {
      window.removeEventListener("pointermove", updatePointer);
      window.removeEventListener("blur", resetPointer);
    };
  }, [interactionRef]);

  useEffect(() => {
    const currentModel = modelRef.current;
    const animation = animationRef.current;

    if (!revealing) {
      if (zooming) return;
      if (animation.activeTurn) {
        currentModel.turnGroup.rotation[animation.activeTurn.axis] =
          animation.activeTurn.direction * (Math.PI / 2);
        finishTurn(currentModel, animation);
      }
      animation.activeTurn = null;
      animation.completionSent = false;
      animation.elapsed = 0;
      animation.phase = "orbit";
      animation.turns = 0;
      return;
    }

    if (animation.activeTurn) {
      currentModel.turnGroup.rotation[animation.activeTurn.axis] =
        animation.activeTurn.direction * (Math.PI / 2);
      finishTurn(currentModel, animation);
    }

    const normalizedY = THREE.MathUtils.euclideanModulo(currentModel.root.rotation.y + Math.PI, Math.PI * 2) - Math.PI;
    currentModel.root.rotation.y = normalizedY;
    animation.activeTurn = null;
    animation.alignFrom = {
      x: currentModel.root.rotation.x,
      y: normalizedY,
      z: currentModel.root.rotation.z,
    };
    animation.completionSent = false;
    animation.elapsed = 0;
    animation.phase = "align";
    animation.turns = 0;
  }, [revealing, zooming]);

  useEffect(() => {
    const currentModel = modelRef.current;
    const animation = animationRef.current;

    if (zooming) {
      if (animation.activeTurn) {
        currentModel.turnGroup.rotation[animation.activeTurn.axis] =
          animation.activeTurn.direction * (Math.PI / 2);
        finishTurn(currentModel, animation);
      }
      zoomRef.current = {
        elapsed: 0,
        from: camera.position.clone(),
        rootFrom: {
          x: currentModel.root.rotation.x,
          y: currentModel.root.rotation.y,
          z: currentModel.root.rotation.z,
        },
      };
      return;
    }

    if (focused) return;

    if (returning) {
      returnRef.current = {
        elapsed: 0,
        from: camera.position.clone(),
        rootFrom: {
          x: currentModel.root.rotation.x,
          y: currentModel.root.rotation.y,
          z: currentModel.root.rotation.z,
        },
      };
      return;
    }

    if (!zooming) {
      camera.position.set(0, 0, cameraDistance);
      camera.lookAt(0, 0, 0);
      zoomRef.current.elapsed = 0;
    }
  }, [camera, cameraDistance, focusCell.column, focusCell.row, focused, returning, zooming]);

  useEffect(() => () => {
    const model = modelRef.current;
    if (!model) return;
    model.geometry.dispose();
    model.edgeGeometry.dispose();
    model.surfaceMaterial.dispose();
    model.edgeMaterial.dispose();
  }, []);

  useFrame((_, frameDelta) => {
    if (paused || reducedMotion) return;

    const model = modelRef.current;
    if (!model) return;
    const delta = Math.min(frameDelta, 0.05);
    const animation = animationRef.current;

    if (zooming) {
      const zoom = zoomRef.current;
      zoom.elapsed += delta;
      const progress = Math.min(zoom.elapsed / FEATURE_ZOOM_DURATION, 1);
      const eased = easeInOutCubic(progress);
      const targetX = (focusCell.column - OUTER_COORDINATE) * CUBIE_SPACING;
      const targetY = (OUTER_COORDINATE - focusCell.row) * CUBIE_SPACING;
      const frontSurface = OUTER_COORDINATE * CUBIE_SPACING + CUBIE_SIZE / 2;
      camera.position.set(
        THREE.MathUtils.lerp(zoom.from.x, targetX, eased),
        THREE.MathUtils.lerp(zoom.from.y, targetY, eased),
        THREE.MathUtils.lerp(zoom.from.z, frontSurface + 1.65, eased),
      );
      camera.lookAt(camera.position.x, camera.position.y, 0);
      model.root.rotation.set(
        THREE.MathUtils.lerp(zoom.rootFrom.x, 0, eased),
        THREE.MathUtils.lerp(zoom.rootFrom.y, 0, eased),
        THREE.MathUtils.lerp(zoom.rootFrom.z, 0, eased),
      );
      return;
    }

    const interaction = interactionRef.current;
    const pointerX = interaction.dragging ? 0 : interaction.pointer.x;
    const pointerY = interaction.dragging ? 0 : interaction.pointer.y;
    const targetRootX = interaction.base.x + pointerY * 0.23;
    const targetRootY = interaction.base.y + pointerX * 0.32;
    const targetRootZ = interaction.base.z + pointerX * -0.06;

    if (returning) {
      const returningState = returnRef.current;
      returningState.elapsed += delta;
      const progress = Math.min(returningState.elapsed / FEATURE_RETURN_DURATION, 1);
      const eased = easeInOutCubic(progress);
      camera.position.set(
        THREE.MathUtils.lerp(returningState.from.x, 0, eased),
        THREE.MathUtils.lerp(returningState.from.y, 0, eased),
        THREE.MathUtils.lerp(returningState.from.z, cameraDistance, eased),
      );
      camera.lookAt(camera.position.x, camera.position.y, 0);
      model.root.rotation.set(
        THREE.MathUtils.lerp(returningState.rootFrom.x, targetRootX, eased),
        THREE.MathUtils.lerp(returningState.rootFrom.y, targetRootY, eased),
        THREE.MathUtils.lerp(returningState.rootFrom.z, targetRootZ, eased),
      );
      return;
    }

    model.root.rotation.x = THREE.MathUtils.damp(model.root.rotation.x, targetRootX, 6.5, delta);
    model.root.rotation.y = THREE.MathUtils.damp(model.root.rotation.y, targetRootY, 6.5, delta);
    model.root.rotation.z = THREE.MathUtils.damp(model.root.rotation.z, targetRootZ, 6.5, delta);

    if (!solving) {
      return;
    }

    if (revealing) {
      animation.elapsed += delta;

      if (animation.phase === "align") {
        const progress = Math.min(animation.elapsed / REVEAL_ALIGN_DURATION, 1);
        const eased = easeInOutCubic(progress);
        model.root.rotation.set(
          THREE.MathUtils.lerp(animation.alignFrom.x, 0, eased),
          THREE.MathUtils.lerp(animation.alignFrom.y, 0, eased),
          THREE.MathUtils.lerp(animation.alignFrom.z, 0, eased),
        );
        if (progress >= 1) {
          model.root.rotation.set(0, 0, 0);
          animation.elapsed = 0;
          animation.phase = "gap";
        }
        return;
      }

      if (animation.phase === "gap") {
        if (animation.elapsed < REVEAL_TURN_GAP) return;
        if (animation.turns >= TURNS_PER_SEQUENCE) {
          animation.elapsed = 0;
          animation.phase = "hold";
          if (!animation.completionSent) {
            animation.completionSent = true;
            onRevealCompleteRef.current?.();
          }
          return;
        }
        beginRandomTurn(model, animation);
        return;
      }

      if (animation.phase === "turn" && animation.activeTurn) {
        const progress = Math.min(animation.elapsed / REVEAL_TURN_DURATION, 1);
        model.turnGroup.rotation[animation.activeTurn.axis] =
          animation.activeTurn.direction * (Math.PI / 2) * easeInOutCubic(progress);
        if (progress >= 1) finishTurn(model, animation);
      }
      return;
    }

    animation.elapsed += delta;

    if (animation.phase === "orbit") {
      if (animation.elapsed >= ORBIT_DURATION) {
        animation.elapsed = 0;
        animation.turns = 0;
        animation.phase = "gap";
      }
      return;
    }

    if (animation.phase === "gap") {
      if (animation.elapsed < TURN_GAP) return;
      if (animation.turns >= TURNS_PER_SEQUENCE) {
        animation.elapsed = 0;
        animation.phase = "orbit";
        return;
      }
      beginRandomTurn(model, animation);
      return;
    }

    if (!animation.activeTurn) return;
    const progress = Math.min(animation.elapsed / TURN_DURATION, 1);
    model.turnGroup.rotation[animation.activeTurn.axis] =
      animation.activeTurn.direction * (Math.PI / 2) * easeInOutCubic(progress);

    if (progress >= 1) finishTurn(model, animation);
  });

  return <primitive object={model.root} />;
}

export function CubeMark({
  cameraDistance = 17,
  className = "",
  focusCell = { column: 2, row: 2 },
  focused = false,
  interactive = false,
  onRevealComplete,
  paused = false,
  revealing = false,
  returning = false,
  size = 88,
  solving = false,
  zooming = false,
}: CubeMarkProps) {
  const interactionRef = useRef<InteractionState>({
    base: { x: 0.34, y: -0.55, z: 0 },
    dragging: false,
    lastX: 0,
    lastY: 0,
    pointer: new THREE.Vector2(),
  });
  const reducedMotion = typeof window !== "undefined" && typeof window.matchMedia === "function" &&
    window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  const style = { "--cube-mark-size": `${size}px` } as CSSProperties;
  const classes = [
    "cube-mark",
    className,
    solving ? "cube-mark--solving" : "",
    paused ? "cube-mark--paused" : "",
    interactive ? "cube-mark--interactive" : "",
  ]
    .filter(Boolean)
    .join(" ");

  const startDrag = (event: ReactPointerEvent<HTMLSpanElement>) => {
    if (!interactive || event.button !== 0) return;
    event.preventDefault();
    interactionRef.current.dragging = true;
    interactionRef.current.lastX = event.clientX;
    interactionRef.current.lastY = event.clientY;
    event.currentTarget.setPointerCapture(event.pointerId);
  };

  const drag = (event: ReactPointerEvent<HTMLSpanElement>) => {
    const interaction = interactionRef.current;
    if (!interactive || !interaction.dragging) return;
    const deltaX = event.clientX - interaction.lastX;
    const deltaY = event.clientY - interaction.lastY;
    interaction.lastX = event.clientX;
    interaction.lastY = event.clientY;
    interaction.base.x = THREE.MathUtils.clamp(interaction.base.x + deltaY * 0.008, -1.15, 1.15);
    interaction.base.y += deltaX * 0.008;
  };

  const finishDrag = (event: ReactPointerEvent<HTMLSpanElement>) => {
    if (!interactionRef.current.dragging) return;
    interactionRef.current.dragging = false;
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId);
    }
  };

  return (
    <span className={classes} style={style} aria-hidden="true">
      {interactive ? (
        <span
          className="cube-mark__drag-surface"
          onPointerCancel={finishDrag}
          onPointerDown={startDrag}
          onPointerMove={drag}
          onPointerUp={finishDrag}
        />
      ) : null}
      <Canvas
        camera={{ fov: 30, position: [0, 0, cameraDistance] }}
        className="cube-mark__canvas"
        dpr={[1, 1.5]}
        frameloop={paused || reducedMotion ? "demand" : "always"}
        gl={{ alpha: true, antialias: true, powerPreference: "high-performance" }}
      >
        <PuzzleCube
          cameraDistance={cameraDistance}
          focusCell={focusCell}
          focused={focused}
          interactionRef={interactionRef}
          onRevealComplete={onRevealComplete}
          paused={paused}
          reducedMotion={reducedMotion}
          revealing={revealing}
          returning={returning}
          solving={solving}
          zooming={zooming}
        />
      </Canvas>
    </span>
  );
}
