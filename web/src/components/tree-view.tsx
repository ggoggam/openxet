"use client";

import {
  createContext,
  useContext,
  useId,
  useMemo,
  useCallback,
  useEffect,
  useRef,
} from "react";
import { DragDropProvider, DragOverlay } from "@dnd-kit/react";
import { cn } from "@/lib/utils";
import type {
  TreeNodeData,
  TreeNodeNested,
  FlatTreeNode,
  TreeDragEvent,
  TreeNodeRenderProps,
  LoadChildrenFn,
  DropPosition,
  MaybePromise,
} from "@/lib/tree-types";
import { TreeViewProvider } from "@/lib/tree-context";
import { useTreeState } from "@/hooks/use-tree-state";
import { useTreeKeyboard } from "@/hooks/use-tree-keyboard";
import { useTreeLazy } from "@/hooks/use-tree-lazy";
import { useTreeDnd } from "@/hooks/use-tree-dnd";
import { TreeNodeRow } from "@/components/tree-node";

// ---------- DND Group Context ----------

interface DndGroupRegistration {
  treeId: string;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  handleDragStart: (...args: any[]) => void;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  handleDragOver: (...args: any[]) => void;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  handleDragEnd: (...args: any[]) => void;
  flatNodes: FlatTreeNode[];
  /** Current DND projection state (set during handleDragOver) */
  overId: string | null;
  dropPosition: DropPosition | null;
  projectedDepth: number | null;
  projectedParentId: string | null;
}

interface DndGroupContextValue {
  register: (reg: DndGroupRegistration) => void;
  unregister: (treeId: string) => void;
}

/**
 * Internal context for cross-tree DND registration.
 * null when not inside a TreeViewDndContext.
 */
const TreeViewDndGroupContext = createContext<DndGroupContextValue | null>(
  null,
);

// ---------- TreeView Props ----------

export interface TreeViewProps<
  T extends TreeNodeData = TreeNodeData,
> extends Omit<
  React.ComponentProps<"div">,
  "onChange" | "onDragStart" | "onDragEnd" | "onDragOver"
> {
  /** Tree data in nested format */
  items: TreeNodeNested<T>[];
  /** Called when tree structure changes (reorder, DND) */
  onItemsChange?: (items: TreeNodeNested<T>[]) => MaybePromise<void>;
  /** Unique ID for this tree instance (auto-generated if not provided) */
  treeId?: string;
  /** Render function for each tree node */
  renderNode: (props: TreeNodeRenderProps<T>) => React.ReactNode;
  /** Render function for the drag overlay */
  renderDragOverlay?: (props: TreeNodeRenderProps<T>) => React.ReactNode;
  /** Async function to load children on expand */
  loadChildren?: LoadChildrenFn<T>;
  /** Error callback for lazy loading failures */
  onLoadError?: (nodeId: string, error: Error) => MaybePromise<void>;
  /** Selection mode */
  selectionMode?: "none" | "single" | "multiple";
  /** Controlled selected IDs */
  selectedIds?: string[];
  /** Selection change callback */
  onSelectedIdsChange?: (ids: string[]) => MaybePromise<void>;
  /** Controlled expanded IDs */
  expandedIds?: string[];
  /** Expansion change callback */
  onExpandedIdsChange?: (ids: string[]) => MaybePromise<void>;
  /** Expand all by default */
  defaultExpandAll?: boolean;
  /** Specific IDs to expand by default */
  defaultExpandedIds?: string[];
  /** Enable drag */
  draggable?: boolean;
  /** Enable drop */
  droppable?: boolean;
  /** Per-node drag guard */
  canDrag?: (node: FlatTreeNode<T>) => boolean;
  /** Per-operation drop guard */
  canDrop?: (event: TreeDragEvent<T>) => boolean;
  /** Auto-expand nodes on drag hover */
  expandOnDragHover?: boolean;
  /** Delay before auto-expand (ms) */
  expandOnDragHoverDelay?: number;
  /** Pixels per indent level */
  indentationWidth?: number;
  /**
   * Horizontal offset (in px) from the left edge of each indentation column
   * to the guide line. The guide line for depth `d` is positioned at
   * `d * indentationWidth + guideLineOffset`.
   *
   * Set this to match the center of your chevron/toggle element.
   * Default: 16 (aligns with 8px base padding + center of a 16px icon container).
   */
  guideLineOffset?: number;
  /** Show vertical guide lines for nesting depth (default: true) */
  showGuideLines?: boolean;
  /**
   * Shared DND group identifier for cross-tree drag-and-drop.
   * Trees with the same dndGroup can exchange items.
   * Defaults to the tree's own treeId (no cross-tree).
   */
  dndGroup?: string;
  /** DND event callbacks */
  onDragStart?: (event: TreeDragEvent<T>) => MaybePromise<void>;
  onDragEnd?: (event: TreeDragEvent<T>) => MaybePromise<void>;
}

// ---------- TreeView Component ----------

export function TreeView<T extends TreeNodeData = TreeNodeData>({
  items,
  onItemsChange,
  treeId: treeIdProp,
  renderNode,
  renderDragOverlay,
  loadChildren,
  onLoadError,
  selectionMode = "single",
  selectedIds: selectedIdsProp,
  onSelectedIdsChange,
  expandedIds: expandedIdsProp,
  onExpandedIdsChange,
  defaultExpandAll,
  defaultExpandedIds,
  draggable = false,
  droppable = false,
  canDrag,
  canDrop,
  expandOnDragHover = true,
  expandOnDragHoverDelay = 500,
  indentationWidth = 20,
  guideLineOffset = 16,
  showGuideLines = true,
  dndGroup: dndGroupProp,
  onDragStart,
  onDragEnd,
  className,
  ...divProps
}: TreeViewProps<T>) {
  const autoId = useId();
  const treeId = treeIdProp ?? autoId;
  const dndGroupCtx = useContext(TreeViewDndGroupContext);
  const isInsideDndGroup = dndGroupCtx !== null;
  const dndGroup = dndGroupProp ?? treeId;

  // Core state
  const state = useTreeState<T>({
    items,
    onItemsChange,
    selectionMode,
    selectedIds: selectedIdsProp,
    onSelectedIdsChange,
    expandedIds: expandedIdsProp,
    onExpandedIdsChange,
    defaultExpandAll,
    defaultExpandedIds,
  });

  // Lazy loading
  const lazy = useTreeLazy<T>({
    loadChildren,
    insertChildren: state.insertChildren,
    expand: state.expand,
    onLoadError,
  });

  // DND
  const dnd = useTreeDnd<T>({
    treeId,
    flatNodes: state.flatNodes,
    visibleNodes: state.visibleNodes,
    expandedIds: state.expandedIds,
    selectedIds: state.selectedIds,
    selectionMode,
    indentationWidth,
    canDrag,
    canDrop,
    onItemsChange,
    onDragStart,
    onDragEnd,
    expandOnDragHover,
    expandOnDragHoverDelay,
    expand: state.expand,
  });

  // Register with group context for cross-tree DND event dispatch
  const dndHandlersRef = useRef(dnd);
  dndHandlersRef.current = dnd;
  const flatNodesRef = useRef(state.flatNodes);
  flatNodesRef.current = state.flatNodes;

  useEffect(() => {
    if (!dndGroupCtx || !(draggable || droppable)) return;
    const reg: DndGroupRegistration = {
      treeId,
      get handleDragStart() {
        return dndHandlersRef.current.handleDragStart;
      },
      get handleDragOver() {
        return dndHandlersRef.current.handleDragOver;
      },
      get handleDragEnd() {
        return dndHandlersRef.current.handleDragEnd;
      },
      get flatNodes() {
        return flatNodesRef.current as FlatTreeNode[];
      },
      get overId() {
        return dndHandlersRef.current.overId;
      },
      get dropPosition() {
        return dndHandlersRef.current.dropPosition;
      },
      get projectedDepth() {
        return dndHandlersRef.current.projectedDepth;
      },
      get projectedParentId() {
        return dndHandlersRef.current.projectedParentId;
      },
    };
    dndGroupCtx.register(reg);
    return () => dndGroupCtx.unregister(treeId);
  }, [dndGroupCtx, treeId, draggable, droppable]);

  // Override toggleExpand to trigger lazy loading
  const toggleExpand = useCallback(
    (id: string) => {
      const node = state.flatNodes.find((n) => n.id === id);
      if (node && node.isGroup && !node.childrenLoaded && loadChildren) {
        lazy.triggerLoad(node);
      } else {
        state.toggleExpand(id);
      }
    },
    [state, lazy, loadChildren],
  );

  // Keyboard navigation
  const keyboard = useTreeKeyboard<T>({
    visibleNodes: state.visibleNodes,
    flatNodes: state.flatNodes,
    focusedId: state.focusedId,
    expandedIds: state.expandedIds,
    setFocused: state.setFocused,
    toggleExpand,
    expand: state.expand,
    collapse: state.collapse,
    select: state.select,
    toggleSelect: state.toggleSelect,
    selectRange: state.selectRange,
    selectAll: state.selectAll,
    selectionMode,
  });

  // Active node for drag overlay
  const activeNode = dnd.activeId
    ? (state.flatNodes.find((n) => n.id === dnd.activeId) ?? null)
    : null;

  // Context value
  const contextValue = useMemo(
    () => ({
      treeId,
      dndGroup,
      flatNodes: state.flatNodes,
      visibleNodes: state.visibleNodes,
      expandedIds: state.expandedIds,
      selectedIds: state.selectedIds,
      focusedId: state.focusedId,
      loadingIds: lazy.loadingIds,
      activeId: dnd.activeId,
      overId: dnd.overId,
      dropPosition: dnd.dropPosition,
      projectedDepth: dnd.projectedDepth,
      indentationWidth,
      selectionMode,
      guideLineOffset,
      showGuideLines,
      draggable,
      droppable,
      canDrag,
      toggleExpand,
      select: state.select,
      toggleSelect: state.toggleSelect,
      selectRange: state.selectRange,
      setFocused: state.setFocused,
      renderNode,
      renderDragOverlay,
    }),
    [
      treeId,
      dndGroup,
      state.flatNodes,
      state.visibleNodes,
      state.expandedIds,
      state.selectedIds,
      state.focusedId,
      lazy.loadingIds,
      dnd.activeId,
      dnd.overId,
      dnd.dropPosition,
      dnd.projectedDepth,
      indentationWidth,
      selectionMode,
      guideLineOffset,
      showGuideLines,
      draggable,
      droppable,
      canDrag,
      toggleExpand,
      state.select,
      state.toggleSelect,
      state.selectRange,
      state.setFocused,
      renderNode,
      renderDragOverlay,
    ],
  );

  const treeContent = (
    <TreeViewProvider value={contextValue}>
      <div
        {...divProps}
        role="tree"
        aria-label={divProps["aria-label"]}
        aria-labelledby={divProps["aria-labelledby"]}
        aria-multiselectable={selectionMode === "multiple" || undefined}
        aria-activedescendant={
          state.focusedId ? `${treeId}-node-${state.focusedId}` : undefined
        }
        tabIndex={0}
        data-slot="tree-view"
        className={cn("outline-none", className)}
        onKeyDown={keyboard.onKeyDown}
      >
        {state.visibleNodes.map((node, idx) => (
          <TreeNodeRow<T> key={node.id} node={node} sortableIndex={idx} />
        ))}
      </div>
    </TreeViewProvider>
  );

  const dragOverlayContent =
    renderDragOverlay && activeNode ? (
      <DragOverlay>
        {renderDragOverlay({
          node: activeNode,
          isExpanded: state.expandedIds.has(activeNode.id),
          isSelected: state.selectedIds.has(activeNode.id),
          isFocused: false,
          isLoading: false,
          isDragging: true,
          isDropTarget: false,
          dropPosition: null,
          depth: activeNode.depth,
          hasChildren: activeNode.isGroup,
          selectionMode,
          toggle: () => {},
          select: () => {},
        })}
      </DragOverlay>
    ) : null;

  // When inside a TreeViewDndContext, skip wrapping with our own DragDropProvider
  // — the shared provider already exists above us.
  if (isInsideDndGroup && (draggable || droppable)) {
    return (
      <>
        {treeContent}
        {dragOverlayContent}
      </>
    );
  }

  // Wrap with DragDropProvider if DND is enabled
  if (draggable || droppable) {
    return (
      <DragDropProvider
        onDragStart={dnd.handleDragStart}
        onDragOver={dnd.handleDragOver}
        onDragEnd={dnd.handleDragEnd}
      >
        {treeContent}
        {dragOverlayContent}
      </DragDropProvider>
    );
  }

  return treeContent;
}

// ---------- TreeViewDndContext ----------

/** Extra projection data attached to cross-tree drag end events. */
export interface CrossTreeDragInfo {
  sourceTreeId: string;
  targetTreeId: string;
  dropPosition: DropPosition | null;
  projectedDepth: number | null;
  projectedParentId: string | null;
}

export interface TreeViewDndContextProps {
  children: React.ReactNode;
  onDragStart?: (event: {
    operation: { source: unknown; target: unknown };
  }) => void;
  onDragOver?: (event: {
    operation: { source: unknown; target: unknown };
  }) => void;
  /**
   * Called when a drag ends. For cross-tree moves, a `crossTree` property
   * is attached with the target tree's projection (dropPosition, projectedDepth,
   * projectedParentId) so the consumer can correctly place the node.
   */
  onDragEnd?: (event: {
    operation: { source: unknown; target: unknown };
    canceled: boolean;
    crossTree?: CrossTreeDragInfo;
  }) => void;
}

/**
 * Wraps multiple TreeView instances to enable cross-tree drag-and-drop.
 * Child TreeView instances should still set `draggable`/`droppable` to enable
 * per-node drag capabilities — the shared provider is handled here.
 */
export function TreeViewDndContext({
  children,
  onDragStart,
  onDragOver,
  onDragEnd,
}: TreeViewDndContextProps) {
  const registrationsRef = useRef(new Map<string, DndGroupRegistration>());

  const groupCtx = useMemo<DndGroupContextValue>(
    () => ({
      register: (reg) => {
        registrationsRef.current.set(reg.treeId, reg);
      },
      unregister: (treeId) => {
        registrationsRef.current.delete(treeId);
      },
    }),
    [],
  );

  // Find which registered tree owns a given node id
  const findOwnerTree = useCallback(
    (nodeId: string): DndGroupRegistration | undefined => {
      for (const reg of registrationsRef.current.values()) {
        if (reg.flatNodes.some((n) => n.id === nodeId)) return reg;
      }
      return undefined;
    },
    [],
  );

  const handleDragStart = useCallback(
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    (event: any) => {
      const sourceId = event.operation?.source?.id;
      if (sourceId != null) {
        // Notify the source tree so it updates activeId
        const owner = findOwnerTree(String(sourceId));
        owner?.handleDragStart(event);
      }
      onDragStart?.(event);
    },
    [findOwnerTree, onDragStart],
  );

  const lastOverTreeRef = useRef<string | null>(null);

  const handleDragOver = useCallback(
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    (event: any) => {
      const targetId = event.operation?.target?.id;
      if (targetId != null) {
        const owner = findOwnerTree(String(targetId));
        if (owner) {
          // If target moved to a different tree, clear the previous tree's hover state
          if (
            lastOverTreeRef.current &&
            lastOverTreeRef.current !== owner.treeId
          ) {
            const prev = registrationsRef.current.get(lastOverTreeRef.current);
            if (prev) {
              // Send a synthetic event with no target to clear overId
              prev.handleDragOver({
                ...event,
                operation: { ...event.operation, target: null },
              });
            }
          }
          lastOverTreeRef.current = owner.treeId;
          owner.handleDragOver(event);
        }
      }
      onDragOver?.(event);
    },
    [findOwnerTree, onDragOver],
  );

  const handleDragEnd = useCallback(
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    (event: any) => {
      const sourceId = event.operation?.source?.id;
      const targetId = event.operation?.target?.id;

      if (sourceId != null && targetId != null) {
        const sourceTree = findOwnerTree(String(sourceId));
        const targetTree = findOwnerTree(String(targetId));

        if (
          sourceTree &&
          targetTree &&
          sourceTree.treeId === targetTree.treeId
        ) {
          // Same-tree move — delegate to that tree's handler
          sourceTree.handleDragEnd(event);
        } else if (sourceTree && targetTree) {
          // Cross-tree move — capture the target tree's projection before
          // resetting state, then pass it to the consumer.
          const crossTree: CrossTreeDragInfo = {
            sourceTreeId: sourceTree.treeId,
            targetTreeId: targetTree.treeId,
            dropPosition: targetTree.dropPosition,
            projectedDepth: targetTree.projectedDepth,
            projectedParentId: targetTree.projectedParentId,
          };

          // Reset both trees' DND state
          sourceTree.handleDragEnd({ ...event, canceled: true });
          targetTree.handleDragEnd({ ...event, canceled: true });

          lastOverTreeRef.current = null;
          onDragEnd?.({ ...event, crossTree });
          return;
        }
      } else {
        // No valid source/target — reset all trees
        for (const reg of registrationsRef.current.values()) {
          reg.handleDragEnd({ ...event, canceled: true });
        }
      }

      lastOverTreeRef.current = null;
      onDragEnd?.(event);
    },
    [findOwnerTree, onDragEnd],
  );

  return (
    <TreeViewDndGroupContext.Provider value={groupCtx}>
      <DragDropProvider
        onDragStart={handleDragStart}
        onDragOver={handleDragOver}
        onDragEnd={handleDragEnd}
      >
        {children}
      </DragDropProvider>
    </TreeViewDndGroupContext.Provider>
  );
}

// Re-export types for consumers
export type {
  TreeNodeData,
  TreeNodeNested,
  FlatTreeNode,
  TreeDragEvent,
  TreeNodeRenderProps,
  LoadChildrenFn,
  DropPosition,
  MaybePromise,
} from "@/lib/tree-types";
