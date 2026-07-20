"use client";

import { useState, useMemo, useCallback, useRef } from "react";
import type { TreeNodeData, TreeNodeNested, FlatTreeNode, MaybePromise } from "@/lib/tree-types";
import {
  flattenTree,
  buildTree,
  getVisibleNodes,
  getDescendantIds,
} from "@/lib/tree-utils";

export interface UseTreeStateOptions<T extends TreeNodeData> {
  items: TreeNodeNested<T>[];
  onItemsChange?: (items: TreeNodeNested<T>[]) => MaybePromise<void>;
  selectionMode?: "none" | "single" | "multiple";
  selectedIds?: string[];
  onSelectedIdsChange?: (ids: string[]) => MaybePromise<void>;
  expandedIds?: string[];
  onExpandedIdsChange?: (ids: string[]) => MaybePromise<void>;
  defaultExpandAll?: boolean;
  defaultExpandedIds?: string[];
}

export interface UseTreeStateReturn<T extends TreeNodeData> {
  flatNodes: FlatTreeNode<T>[];
  visibleNodes: FlatTreeNode<T>[];
  expandedIds: Set<string>;
  selectedIds: Set<string>;
  focusedId: string | null;
  toggleExpand: (id: string) => void;
  expand: (id: string) => void;
  collapse: (id: string) => void;
  select: (id: string) => void;
  toggleSelect: (id: string) => void;
  selectRange: (id: string) => void;
  selectAll: () => void;
  setFocused: (id: string | null) => void;
  setFlatNodes: (nodes: FlatTreeNode<T>[]) => void;
  insertChildren: (parentId: string, children: TreeNodeNested<T>[]) => void;
}

function collectAllGroupIds<T extends TreeNodeData>(
  nodes: TreeNodeNested<T>[]
): string[] {
  const ids: string[] = [];
  for (const node of nodes) {
    if (node.isGroup || (node.children && node.children.length > 0)) {
      ids.push(node.id);
    }
    if (node.children) {
      ids.push(...collectAllGroupIds(node.children));
    }
  }
  return ids;
}

export function useTreeState<T extends TreeNodeData>(
  options: UseTreeStateOptions<T>
): UseTreeStateReturn<T> {
  const {
    items,
    onItemsChange,
    selectionMode = "none",
    selectedIds: controlledSelectedIds,
    onSelectedIdsChange,
    expandedIds: controlledExpandedIds,
    onExpandedIdsChange,
    defaultExpandAll = false,
    defaultExpandedIds,
  } = options;

  // Flatten input items
  const flatNodes = useMemo(() => flattenTree(items), [items]);

  // Expanded state (controlled or uncontrolled)
  const [internalExpandedIds, setInternalExpandedIds] = useState<Set<string>>(
    () => {
      if (defaultExpandAll) {
        return new Set(collectAllGroupIds(items));
      }
      if (defaultExpandedIds) {
        return new Set(defaultExpandedIds);
      }
      return new Set<string>();
    }
  );

  const expandedIds = useMemo(
    () =>
      controlledExpandedIds ? new Set(controlledExpandedIds) : internalExpandedIds,
    [controlledExpandedIds, internalExpandedIds]
  );

  // Chain same-tick updates through a ref: evaluating updaters against the
  // render-time snapshot makes multiple calls in one tick last-write-wins
  // (e.g. the * key expanding every sibling would only expand the last one).
  // Only chain in uncontrolled mode — a controlled parent may reject a change
  // without re-rendering, and an optimistically advanced ref would leak that
  // phantom value into every later updater base.
  const expandedIdsRef = useRef(expandedIds);
  expandedIdsRef.current = expandedIds;

  const setExpandedIds = useCallback(
    (updater: Set<string> | ((prev: Set<string>) => Set<string>)) => {
      const next =
        typeof updater === "function" ? updater(expandedIdsRef.current) : updater;
      if (onExpandedIdsChange) {
        onExpandedIdsChange(Array.from(next));
      }
      if (!controlledExpandedIds) {
        expandedIdsRef.current = next;
        setInternalExpandedIds(next);
      }
    },
    [onExpandedIdsChange, controlledExpandedIds]
  );

  // Selected state (controlled or uncontrolled)
  const [internalSelectedIds, setInternalSelectedIds] = useState<Set<string>>(
    new Set()
  );

  const selectedIds = useMemo(
    () =>
      controlledSelectedIds ? new Set(controlledSelectedIds) : internalSelectedIds,
    [controlledSelectedIds, internalSelectedIds]
  );

  // Same uncontrolled-only chaining as expandedIdsRef above.
  const selectedIdsRef = useRef(selectedIds);
  selectedIdsRef.current = selectedIds;

  const setSelectedIds = useCallback(
    (updater: Set<string> | ((prev: Set<string>) => Set<string>)) => {
      const next =
        typeof updater === "function" ? updater(selectedIdsRef.current) : updater;
      if (onSelectedIdsChange) {
        onSelectedIdsChange(Array.from(next));
      }
      if (!controlledSelectedIds) {
        selectedIdsRef.current = next;
        setInternalSelectedIds(next);
      }
    },
    [onSelectedIdsChange, controlledSelectedIds]
  );

  // Focused state
  const [focusedId, setFocused] = useState<string | null>(null);

  // Visible nodes (derived)
  const visibleNodes = useMemo(
    () => getVisibleNodes(flatNodes, expandedIds),
    [flatNodes, expandedIds]
  );

  // Actions
  const toggleExpand = useCallback(
    (id: string) => {
      setExpandedIds((prev) => {
        const next = new Set(prev);
        if (next.has(id)) {
          next.delete(id);
          // Also collapse descendants
          const descendants = getDescendantIds(flatNodes, id);
          for (const d of descendants) next.delete(d);
        } else {
          next.add(id);
        }
        return next;
      });
    },
    [flatNodes, setExpandedIds]
  );

  const expand = useCallback(
    (id: string) => {
      setExpandedIds((prev) => {
        if (prev.has(id)) return prev;
        const next = new Set(prev);
        next.add(id);
        return next;
      });
    },
    [setExpandedIds]
  );

  const collapse = useCallback(
    (id: string) => {
      setExpandedIds((prev) => {
        if (!prev.has(id)) return prev;
        const next = new Set(prev);
        next.delete(id);
        const descendants = getDescendantIds(flatNodes, id);
        for (const d of descendants) next.delete(d);
        return next;
      });
    },
    [flatNodes, setExpandedIds]
  );

  // Anchor for Shift+Click range selection
  const lastSelectedIdRef = useRef<string | null>(null);

  const select = useCallback(
    (id: string) => {
      if (selectionMode === "none") return;
      setSelectedIds(new Set([id]));
      lastSelectedIdRef.current = id;
    },
    [selectionMode, setSelectedIds]
  );

  const toggleSelect = useCallback(
    (id: string) => {
      if (selectionMode === "none") return;
      if (selectionMode === "single") {
        setSelectedIds((prev) =>
          prev.has(id) ? new Set() : new Set([id])
        );
      } else {
        setSelectedIds((prev) => {
          const next = new Set(prev);
          if (next.has(id)) {
            next.delete(id);
          } else {
            next.add(id);
          }
          return next;
        });
      }
      lastSelectedIdRef.current = id;
    },
    [selectionMode, setSelectedIds]
  );

  const selectRange = useCallback(
    (id: string) => {
      if (selectionMode !== "multiple") return;
      const anchor = lastSelectedIdRef.current;
      if (!anchor) {
        // No anchor — treat as plain select
        setSelectedIds(new Set([id]));
        lastSelectedIdRef.current = id;
        return;
      }
      const anchorIdx = visibleNodes.findIndex((n) => n.id === anchor);
      const targetIdx = visibleNodes.findIndex((n) => n.id === id);
      if (anchorIdx === -1 || targetIdx === -1) {
        setSelectedIds(new Set([id]));
        lastSelectedIdRef.current = id;
        return;
      }
      const start = Math.min(anchorIdx, targetIdx);
      const end = Math.max(anchorIdx, targetIdx);
      const rangeIds = new Set<string>();
      for (let i = start; i <= end; i++) {
        rangeIds.add(visibleNodes[i].id);
      }
      setSelectedIds(rangeIds);
      // Do NOT update lastSelectedIdRef — anchor stays for subsequent Shift+Clicks
    },
    [selectionMode, visibleNodes, setSelectedIds]
  );

  const selectAll = useCallback(() => {
    if (selectionMode !== "multiple") return;
    setSelectedIds(new Set(visibleNodes.map((n) => n.id)));
  }, [selectionMode, visibleNodes, setSelectedIds]);

  // Mutation methods for DND and lazy loading
  const onItemsChangeRef = useRef(onItemsChange);
  onItemsChangeRef.current = onItemsChange;

  // Read items through a ref so callbacks captured by in-flight promises
  // (lazy loads) operate on the latest tree, not a snapshot from when the
  // load started. Two loads resolving between renders can still race — the
  // airtight fix would be an updater-style onItemsChange.
  const itemsRef = useRef(items);
  itemsRef.current = items;

  const setFlatNodes = useCallback(
    (nodes: FlatTreeNode<T>[]) => {
      if (onItemsChangeRef.current) {
        onItemsChangeRef.current(buildTree(nodes));
      }
    },
    []
  );

  const insertChildren = useCallback(
    (parentId: string, children: TreeNodeNested<T>[]) => {
      // Rebuild items with children inserted under the parent
      function insertInto(
        nodes: TreeNodeNested<T>[]
      ): TreeNodeNested<T>[] {
        return nodes.map((node) => {
          if (node.id === parentId) {
            return {
              ...node,
              children: [...(node.children ?? []), ...children],
            };
          }
          if (node.children) {
            return { ...node, children: insertInto(node.children) };
          }
          return node;
        });
      }

      if (onItemsChangeRef.current) {
        onItemsChangeRef.current(insertInto(itemsRef.current));
      }
    },
    []
  );

  return {
    flatNodes,
    visibleNodes,
    expandedIds,
    selectedIds,
    focusedId,
    toggleExpand,
    expand,
    collapse,
    select,
    toggleSelect,
    selectRange,
    selectAll,
    setFocused,
    setFlatNodes,
    insertChildren,
  };
}
