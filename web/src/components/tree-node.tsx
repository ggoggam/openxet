"use client";

import { useMemo, useCallback } from "react";
import { useSortable } from "@dnd-kit/react/sortable";
import { cn } from "@/lib/utils";
import { useTreeViewContext } from "@/lib/tree-context";
import { TreeDropIndicator } from "@/components/tree-drop-indicator";
import type { TreeNodeData, FlatTreeNode } from "@/lib/tree-types";

interface TreeNodeRowProps<T extends TreeNodeData = TreeNodeData> {
  node: FlatTreeNode<T>;
  sortableIndex: number;
}

export function TreeNodeRow<T extends TreeNodeData = TreeNodeData>({
  node,
  sortableIndex,
}: TreeNodeRowProps<T>) {
  const ctx = useTreeViewContext<T>();
  const {
    treeId,
    dndGroup,
    flatNodes,
    visibleNodes,
    expandedIds,
    selectedIds,
    focusedId,
    loadingIds,
    overId,
    dropPosition,
    projectedDepth,
    indentationWidth,
    selectionMode,
    guideLineOffset,
    showGuideLines,
    draggable: isDraggableTree,
    canDrag,
    toggleExpand,
    select,
    toggleSelect,
    selectRange,
    renderNode,
  } = ctx;

  const isExpanded = expandedIds.has(node.id);
  const isSelected = selectedIds.has(node.id);
  const isFocused = focusedId === node.id;
  const isLoading = loadingIds.has(node.id);
  const isDropTargetNode = overId === node.id;
  const currentDropPosition = isDropTargetNode ? dropPosition : null;

  const hasChildren =
    node.isGroup &&
    (node.childrenLoaded
      ? flatNodes.some((n) => n.parentId === node.id)
      : true);

  const handleSelect = useCallback(
    (event?: React.MouseEvent) => {
      if (selectionMode === "multiple" && event) {
        if (event.shiftKey) {
          selectRange(node.id);
          return;
        }
        if (event.metaKey || event.ctrlKey) {
          toggleSelect(node.id);
          return;
        }
      }
      select(node.id);
    },
    [selectionMode, node.id, select, toggleSelect, selectRange],
  );

  const isDragDisabled = !isDraggableTree || (canDrag ? !canDrag(node) : false);

  // Count siblings for aria-setsize
  const siblingCount = flatNodes.filter(
    (n) => n.parentId === node.parentId,
  ).length;

  const { ref, isDragSource } = useSortable({
    id: node.id,
    index: sortableIndex,
    group: dndGroup,
    disabled: isDragDisabled,
    data: { node, treeId },
    // Disable the default OptimisticSortingPlugin — we handle reordering
    // manually in handleDragEnd via buildTree. The plugin would otherwise
    // reorder DOM elements and mutate sortable indices during the drag,
    // causing the target in onDragEnd to be stale/incorrect.
    plugins: [],
    transition: null,
  });

  const indicatorDepth = projectedDepth ?? node.depth;

  // Compute which depth levels should show a vertical guide line.
  // A line at level `d` means the ancestor subtree at that depth continues
  // below this node (i.e., the ancestor has more siblings/children after).
  const guideLines = useMemo(() => {
    if (!showGuideLines || node.depth === 0) return [];

    const lines: number[] = [];
    // For each depth level from 0 to node.depth - 1, scan forward in
    // visibleNodes to see if a node at that depth appears before a node at a
    // shallower depth (which would close the subtree).
    const continued = new Set<number>();
    for (let i = sortableIndex + 1; i < visibleNodes.length; i++) {
      const d = visibleNodes[i].depth;
      if (d < node.depth) {
        // We found a node shallower than current — mark this depth as
        // continued (the ancestor at depth `d` has more siblings).
        continued.add(d);
        if (d === 0) break; // no deeper ancestors to check
      }
    }

    for (let d = 0; d < node.depth; d++) {
      if (continued.has(d)) lines.push(d);
    }

    // The immediate parent's line always shows if there is a subsequent
    // sibling or cousin visible at the same depth or a node at parent depth.
    // More accurately: the parent line shows if the current node is NOT the
    // last child of its parent in the visible list.
    const parentDepth = node.depth - 1;
    if (!continued.has(parentDepth)) {
      // Check if there's a subsequent sibling (same parentId)
      for (let i = sortableIndex + 1; i < visibleNodes.length; i++) {
        const n = visibleNodes[i];
        if (n.depth <= parentDepth) break;
        if (n.depth === node.depth && n.parentId === node.parentId) {
          lines.push(parentDepth);
          break;
        }
      }
    }

    return lines;
  }, [showGuideLines, node.depth, node.parentId, sortableIndex, visibleNodes]);

  return (
    <div
      ref={ref}
      role="treeitem"
      id={`${treeId}-node-${node.id}`}
      aria-expanded={node.isGroup ? isExpanded : undefined}
      aria-selected={isSelected}
      aria-level={node.depth + 1}
      aria-setsize={siblingCount}
      aria-posinset={node.index + 1}
      data-slot="tree-node"
      data-node-id={node.id}
      data-depth={node.depth}
      data-dragging={isDragSource || undefined}
      data-drop-target={isDropTargetNode || undefined}
      data-drop-position={currentDropPosition}
      className={cn(
        "relative outline-none",
        isDragSource && "opacity-50",
        isDropTargetNode &&
          currentDropPosition === "inside" &&
          "bg-accent/50 rounded-md",
      )}
    >
      {guideLines.map((d) => (
        <span
          key={d}
          aria-hidden
          data-slot="tree-guide-line"
          className="absolute top-0 bottom-0 w-px bg-border"
          style={{ left: d * indentationWidth + guideLineOffset }}
        />
      ))}
      {renderNode({
        node,
        isExpanded,
        isSelected,
        isFocused,
        isLoading,
        isDragging: isDragSource,
        isDropTarget: isDropTargetNode,
        dropPosition: currentDropPosition,
        depth: node.depth,
        hasChildren,
        selectionMode,
        toggle: () => toggleExpand(node.id),
        select: handleSelect,
      })}
      {isDropTargetNode && currentDropPosition === "after" && (
        <TreeDropIndicator
          depth={indicatorDepth}
          indentationWidth={indentationWidth}
        />
      )}
    </div>
  );
}
