"use client";

import { createContext, useContext } from "react";
import type { TreeNodeData, FlatTreeNode, DropPosition } from "@/lib/tree-types";

/**
 * Internal context for a single TreeView instance.
 * Shared between TreeView and its TreeNode children.
 */
export interface TreeViewContextValue<T extends TreeNodeData = TreeNodeData> {
  treeId: string;
  dndGroup: string;
  flatNodes: FlatTreeNode<T>[];
  visibleNodes: FlatTreeNode<T>[];
  expandedIds: Set<string>;
  selectedIds: Set<string>;
  focusedId: string | null;
  loadingIds: Set<string>;
  activeId: string | null;
  overId: string | null;
  dropPosition: DropPosition | null;
  projectedDepth: number | null;
  indentationWidth: number;
  selectionMode: "none" | "single" | "multiple";
  guideLineOffset: number;
  showGuideLines: boolean;
  draggable: boolean;
  droppable: boolean;
  canDrag?: (node: FlatTreeNode<T>) => boolean;
  toggleExpand: (id: string) => void;
  select: (id: string) => void;
  toggleSelect: (id: string) => void;
  selectRange: (id: string) => void;
  setFocused: (id: string | null) => void;
  renderNode: (
    props: import("./tree-types").TreeNodeRenderProps<T>,
  ) => React.ReactNode;
  renderDragOverlay?: (
    props: import("./tree-types").TreeNodeRenderProps<T>,
  ) => React.ReactNode;
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
const TreeViewContext = createContext<TreeViewContextValue<any> | null>(null);

export const TreeViewProvider = TreeViewContext.Provider;

export function useTreeViewContext<
  T extends TreeNodeData = TreeNodeData,
>(): TreeViewContextValue<T> {
  const ctx = useContext(TreeViewContext);
  if (!ctx) {
    throw new Error("useTreeViewContext must be used within a <TreeView>");
  }
  return ctx as TreeViewContextValue<T>;
}
