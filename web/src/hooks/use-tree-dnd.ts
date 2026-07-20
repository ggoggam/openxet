"use client";

import { useState, useCallback, useRef } from "react";
import type {
  TreeNodeData,
  FlatTreeNode,
  DropPosition,
  TreeDragEvent,
  MaybePromise,
} from "@/lib/tree-types";
import {
  getDescendantIds,
  getProjection,
  removeNodes,
  buildTree,
} from "@/lib/tree-utils";
import type {
  DragStartEvent,
  DragOverEvent,
  DragEndEvent,
} from "@dnd-kit/react";

export interface UseTreeDndOptions<T extends TreeNodeData> {
  treeId: string;
  flatNodes: FlatTreeNode<T>[];
  visibleNodes: FlatTreeNode<T>[];
  expandedIds: Set<string>;
  selectedIds: Set<string>;
  selectionMode: "none" | "single" | "multiple";
  indentationWidth: number;
  canDrag?: (node: FlatTreeNode<T>) => boolean;
  canDrop?: (event: TreeDragEvent<T>) => boolean;
  onItemsChange?: (items: import("../lib/tree-types").TreeNodeNested<T>[]) => MaybePromise<void>;
  onDragStart?: (event: TreeDragEvent<T>) => MaybePromise<void>;
  onDragEnd?: (event: TreeDragEvent<T>) => MaybePromise<void>;
  expandOnDragHover?: boolean;
  expandOnDragHoverDelay?: number;
  expand: (id: string) => void;
}

export interface UseTreeDndReturn {
  activeId: string | null;
  overId: string | null;
  dropPosition: DropPosition | null;
  projectedDepth: number | null;
  projectedParentId: string | null;
  handleDragStart: (event: DragStartEvent) => void;
  handleDragOver: (event: DragOverEvent) => void;
  handleDragEnd: (event: DragEndEvent) => void;
}

export function useTreeDnd<T extends TreeNodeData>(
  options: UseTreeDndOptions<T>
): UseTreeDndReturn {
  const {
    treeId,
    flatNodes,
    visibleNodes,
    expandedIds,
    selectedIds,
    selectionMode,
    indentationWidth,
    canDrop,
    onItemsChange,
    onDragStart,
    onDragEnd,
    expandOnDragHover = true,
    expandOnDragHoverDelay = 500,
    expand,
  } = options;

  const [activeId, setActiveId] = useState<string | null>(null);
  const [overId, setOverId] = useState<string | null>(null);
  const [dropPosition, setDropPosition] = useState<DropPosition | null>(null);
  const [projectedDepth, setProjectedDepth] = useState<number | null>(null);
  const [projectedParentId, setProjectedParentId] = useState<string | null>(
    null
  );

  // Refs to avoid stale closures in handleDragEnd — state updates from
  // handleDragOver may not have committed by the time handleDragEnd fires.
  const dropPositionRef = useRef<DropPosition | null>(null);
  const projectedDepthRef = useRef<number | null>(null);
  const projectedParentIdRef = useRef<string | null>(null);

  const hoverTimerRef = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const hoverTargetRef = useRef<string | null>(null);
  const offsetRef = useRef({ x: 0, y: 0 });

  const handleDragStart = useCallback(
    (event: DragStartEvent) => {
      const source = event.operation.source;
      if (!source) return;

      const sourceNode = flatNodes.find((n) => n.id === source.id);
      if (!sourceNode) return;

      setActiveId(String(source.id));
      offsetRef.current = { x: 0, y: 0 };

      if (onDragStart && sourceNode) {
        onDragStart({
          source: sourceNode,
          sourceTreeId: treeId,
          target: sourceNode,
          targetTreeId: treeId,
          position: "before",
          projectedDepth: sourceNode.depth,
        });
      }
    },
    [flatNodes, treeId, onDragStart]
  );

  const handleDragOver = useCallback(
    (event: DragOverEvent) => {
      // Clear both React state and the refs handleDragEnd reads — clearing
      // only the visuals would let the drop commit with a stale projection.
      const clearProjection = () => {
        setOverId(null);
        setDropPosition(null);
        setProjectedDepth(null);
        setProjectedParentId(null);
        dropPositionRef.current = null;
        projectedDepthRef.current = null;
        projectedParentIdRef.current = null;
        clearTimeout(hoverTimerRef.current);
        hoverTargetRef.current = null;
      };

      const { source, target } = event.operation;
      if (!source || !target) {
        clearProjection();
        return;
      }

      const targetId = String(target.id);
      setOverId(targetId);

      // Track cumulative offset for depth projection
      const transform = event.operation.transform;
      if (transform) {
        offsetRef.current = { x: transform.x, y: transform.y };
      }

      // Calculate projection
      const projection = getProjection(
        flatNodes,
        visibleNodes,
        String(source.id),
        targetId,
        offsetRef.current.x,
        indentationWidth
      );

      // Prevent dropping a node into its own subtree, or onto the dragged
      // subtree itself — handleDragEnd rejects both, so showing an indicator
      // here would promise a drop that silently no-ops.
      // In multi-select mode, also check all selected nodes.
      const sourceId = String(source.id);
      const movingIds =
        selectionMode === "multiple" && selectedIds.has(sourceId)
          ? selectedIds
          : new Set([sourceId]);
      const allIdsToMove = new Set<string>();
      for (const id of movingIds) {
        allIdsToMove.add(id);
        for (const d of getDescendantIds(flatNodes, id)) {
          allIdsToMove.add(d);
        }
      }
      if (
        allIdsToMove.has(targetId) ||
        (projection.parentId !== null && allIdsToMove.has(projection.parentId))
      ) {
        clearProjection();
        return;
      }

      setProjectedDepth(projection.depth);
      setProjectedParentId(projection.parentId);
      projectedDepthRef.current = projection.depth;
      projectedParentIdRef.current = projection.parentId;

      // Determine drop position
      const targetNode = flatNodes.find((n) => n.id === targetId);
      if (targetNode) {
        // Check if the target is an expanded group with no visible children.
        // In that case, hovering over it should always mean "inside" since
        // there are no children to sort between. We also override the
        // projected depth/parent so handleDragEnd places the node correctly.
        const isExpandedEmptyGroup =
          targetNode.isGroup &&
          expandedIds.has(targetId) &&
          !visibleNodes.some((n) => n.parentId === targetId);

        const position: DropPosition =
          targetNode.isGroup &&
          (projection.parentId === targetId || isExpandedEmptyGroup)
            ? "inside"
            : "after";

        if (isExpandedEmptyGroup && position === "inside") {
          const correctedDepth = targetNode.depth + 1;
          setProjectedDepth(correctedDepth);
          setProjectedParentId(targetId);
          projectedDepthRef.current = correctedDepth;
          projectedParentIdRef.current = targetId;
        }

        setDropPosition(position);
        dropPositionRef.current = position;
      }

      // Auto-expand on hover
      if (expandOnDragHover && targetId !== hoverTargetRef.current) {
        clearTimeout(hoverTimerRef.current);
        hoverTargetRef.current = targetId;

        const hoverNode = flatNodes.find((n) => n.id === targetId);
        if (
          hoverNode?.isGroup &&
          !expandedIds.has(targetId) &&
          targetId !== String(source.id)
        ) {
          hoverTimerRef.current = setTimeout(() => {
            expand(targetId);
          }, expandOnDragHoverDelay);
        }
      }
    },
    [
      flatNodes,
      visibleNodes,
      indentationWidth,
      expandedIds,
      selectedIds,
      selectionMode,
      expandOnDragHover,
      expandOnDragHoverDelay,
      expand,
    ]
  );

  const handleDragEnd = useCallback(
    (event: DragEndEvent) => {
      clearTimeout(hoverTimerRef.current);
      hoverTargetRef.current = null;

      const { source, target } = event.operation;
      const canceled = event.canceled;

      // Read from refs to avoid stale closures
      const currentDropPosition = dropPositionRef.current;
      const currentProjectedDepth = projectedDepthRef.current;
      const currentProjectedParentId = projectedParentIdRef.current;

      const resetState = () => {
        setActiveId(null);
        setOverId(null);
        setDropPosition(null);
        setProjectedDepth(null);
        setProjectedParentId(null);
        dropPositionRef.current = null;
        projectedDepthRef.current = null;
        projectedParentIdRef.current = null;
      };

      if (canceled || !source || !target) {
        resetState();
        return;
      }

      // No live projection — the last hover was cleared (e.g. over the
      // dragged subtree itself), so the UI showed no drop target. No-op.
      if (currentDropPosition === null) {
        resetState();
        return;
      }

      const sourceNode = flatNodes.find((n) => n.id === source.id);
      const targetNode = flatNodes.find((n) => n.id === target.id);

      if (!sourceNode || !targetNode) {
        resetState();
        return;
      }

      // Check canDrop
      const dragEvent: TreeDragEvent<T> = {
        source: sourceNode,
        sourceTreeId: treeId,
        target: targetNode,
        targetTreeId: treeId,
        position: currentDropPosition,
        projectedDepth: currentProjectedDepth ?? targetNode.depth,
      };

      if (canDrop && !canDrop(dragEvent)) {
        resetState();
        return;
      }

      // Perform the move
      if (onItemsChange) {
        // Determine which root-level nodes to move. In multiple selection mode,
        // if the dragged node is part of the selection, move all selected nodes.
        // Filter out nodes whose ancestor is also selected (they move with their parent).
        const movingMultiple =
          selectionMode === "multiple" &&
          selectedIds.has(sourceNode.id) &&
          selectedIds.size > 1;

        let rootIdsToMove: string[];
        if (movingMultiple) {
          // Build a set of all selected ids and their descendants
          const allDescendants = new Set<string>();
          for (const id of selectedIds) {
            for (const d of getDescendantIds(flatNodes, id)) {
              allDescendants.add(d);
            }
          }
          // Root-level selected = selected but not a descendant of another selected node
          rootIdsToMove = Array.from(selectedIds).filter(
            (id) => !allDescendants.has(id)
          );
        } else {
          rootIdsToMove = [sourceNode.id];
        }

        // Collect all nodes to move (roots + their descendants), preserving flat order
        const allIdsToMove = new Set<string>();
        for (const rootId of rootIdsToMove) {
          allIdsToMove.add(rootId);
          for (const d of getDescendantIds(flatNodes, rootId)) {
            allIdsToMove.add(d);
          }
        }

        // Dropping onto the dragged subtree itself is a no-op — removing the
        // subtree also removes the target, which would otherwise teleport the
        // moved nodes to the end of the tree.
        if (allIdsToMove.has(String(target.id))) {
          resetState();
          return;
        }

        // Prevent dropping a node into its own descendant (would create a cycle)
        if (
          currentProjectedParentId &&
          allIdsToMove.has(currentProjectedParentId)
        ) {
          resetState();
          return;
        }

        const draggedNodes = flatNodes.filter((n) => allIdsToMove.has(n.id));

        // Remove all dragged nodes from the tree
        const remaining = removeNodes(flatNodes, rootIdsToMove);

        // For each root being moved, update depth/parentId relative to the source node.
        // The source node goes to the projected position; other roots maintain their
        // relative depth offsets from the source.
        const depthDiff = (currentProjectedDepth ?? 0) - sourceNode.depth;
        const updatedDragged = draggedNodes.map((n) => {
          if (rootIdsToMove.includes(n.id)) {
            // Root node of a moved subtree
            return {
              ...n,
              depth: n.depth + depthDiff,
              parentId: currentProjectedParentId,
            };
          }
          // Descendant — just shift depth
          return {
            ...n,
            depth: n.depth + depthDiff,
          };
        });

        // Insert at the correct position
        let insertAt = remaining.length;
        const targetFlatIdx = remaining.findIndex(
          (n) => n.id === target.id
        );
        if (targetFlatIdx >= 0) {
          if (currentDropPosition === "inside") {
            // Insert as first child of the target (right after the target node)
            insertAt = targetFlatIdx + 1;
          } else {
            // "after" — insert after the target and all its descendants
            let i = targetFlatIdx + 1;
            while (
              i < remaining.length &&
              remaining[i].depth > remaining[targetFlatIdx].depth
            ) {
              i++;
            }
            insertAt = i;
          }
        }

        const result = [
          ...remaining.slice(0, insertAt),
          ...updatedDragged,
          ...remaining.slice(insertAt),
        ];

        onItemsChange(buildTree(result));
      }

      if (onDragEnd) {
        onDragEnd(dragEvent);
      }

      resetState();
    },
    [
      flatNodes,
      treeId,
      selectedIds,
      selectionMode,
      canDrop,
      onItemsChange,
      onDragEnd,
    ]
  );

  return {
    activeId,
    overId,
    dropPosition,
    projectedDepth,
    projectedParentId,
    handleDragStart,
    handleDragOver,
    handleDragEnd,
  };
}
