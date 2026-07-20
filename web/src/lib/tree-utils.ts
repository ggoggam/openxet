import type { TreeNodeData, TreeNodeNested, FlatTreeNode } from "@/lib/tree-types";

/**
 * Convert nested tree structure to flat array with parentId references.
 */
export function flattenTree<T extends TreeNodeData>(
  nodes: TreeNodeNested<T>[],
  parentId: string | null = null,
  depth: number = 0
): FlatTreeNode<T>[] {
  const result: FlatTreeNode<T>[] = [];

  for (let i = 0; i < nodes.length; i++) {
    const node = nodes[i];
    const hasChildren = node.children !== undefined && node.children.length > 0;

    result.push({
      id: node.id,
      data: node.data,
      isGroup: node.isGroup ?? hasChildren,
      childrenLoaded: node.children !== undefined,
      parentId,
      depth,
      index: i,
    });

    if (node.children) {
      result.push(...flattenTree(node.children, node.id, depth + 1));
    }
  }

  return result;
}

/**
 * Convert flat array back to nested tree structure.
 */
export function buildTree<T extends TreeNodeData>(
  flatNodes: FlatTreeNode<T>[]
): TreeNodeNested<T>[] {
  const nodeMap = new Map<string, TreeNodeNested<T>>();
  const roots: TreeNodeNested<T>[] = [];

  // Create all nodes first
  for (const flat of flatNodes) {
    nodeMap.set(flat.id, {
      id: flat.id,
      data: flat.data,
      isGroup: flat.isGroup || undefined,
      children: flat.childrenLoaded ? [] : undefined,
    });
  }

  // Build parent-child relationships. Link the map entries themselves —
  // pushing copies would detach children created after the parent was
  // emitted (parents with childrenLoaded:false get their array lazily).
  for (const flat of flatNodes) {
    const node = nodeMap.get(flat.id)!;
    if (flat.parentId === null) {
      roots.push(node);
    } else {
      const parent = nodeMap.get(flat.parentId);
      if (parent) {
        (parent.children ??= []).push(node);
      }
    }
  }

  return roots;
}

/**
 * Get only the visible nodes (ancestors all expanded).
 */
export function getVisibleNodes<T extends TreeNodeData>(
  flatNodes: FlatTreeNode<T>[],
  expandedIds: Set<string>
): FlatTreeNode<T>[] {
  const result: FlatTreeNode<T>[] = [];
  const collapsedAncestors = new Set<string>();

  for (const node of flatNodes) {
    // Skip if any ancestor is collapsed
    if (node.parentId !== null && collapsedAncestors.has(node.parentId)) {
      // This node is hidden; if it's a group, its descendants are also hidden
      collapsedAncestors.add(node.id);
      continue;
    }

    result.push(node);

    // If this node is a group and not expanded, mark it as collapsed ancestor
    if (node.isGroup && !expandedIds.has(node.id)) {
      collapsedAncestors.add(node.id);
    }
  }

  return result;
}

/**
 * Get all descendant IDs of a node.
 */
export function getDescendantIds<T extends TreeNodeData>(
  flatNodes: FlatTreeNode<T>[],
  parentId: string
): string[] {
  const descendants: string[] = [];
  const parentSet = new Set<string>([parentId]);

  for (const node of flatNodes) {
    if (node.parentId !== null && parentSet.has(node.parentId)) {
      descendants.push(node.id);
      parentSet.add(node.id);
    }
  }

  return descendants;
}

/**
 * Get all ancestor IDs of a node (from immediate parent to root).
 */
export function getAncestorIds<T extends TreeNodeData>(
  flatNodes: FlatTreeNode<T>[],
  nodeId: string
): string[] {
  const ancestors: string[] = [];
  const nodeMap = new Map<string, FlatTreeNode<T>>();

  for (const node of flatNodes) {
    nodeMap.set(node.id, node);
  }

  let current = nodeMap.get(nodeId);
  while (current && current.parentId !== null) {
    ancestors.push(current.parentId);
    current = nodeMap.get(current.parentId);
  }

  return ancestors;
}

/**
 * Get the count of siblings for a node (nodes with the same parentId).
 */
export function getSiblingCount<T extends TreeNodeData>(
  flatNodes: FlatTreeNode<T>[],
  parentId: string | null
): number {
  let count = 0;
  for (const node of flatNodes) {
    if (node.parentId === parentId) count++;
  }
  return count;
}

/**
 * Calculate the projected depth and parentId for a DND drop operation.
 * Based on horizontal cursor offset from the active item's original position.
 */
export function getProjection<T extends TreeNodeData>(
  flatNodes: FlatTreeNode<T>[],
  visibleNodes: FlatTreeNode<T>[],
  activeId: string,
  overId: string,
  offsetX: number,
  indentWidth: number
): { depth: number; parentId: string | null } {
  const overIndex = visibleNodes.findIndex((n) => n.id === overId);

  if (overIndex === -1) {
    return { depth: 0, parentId: null };
  }

  const overNode = visibleNodes[overIndex];
  const activeNode = flatNodes.find((n) => n.id === activeId);

  // When the source node isn't in this tree (cross-tree drag),
  // use the over node's depth as the base for projection.
  const baseDepth = activeNode ? activeNode.depth : overNode.depth;
  const depthDelta = Math.round(offsetX / indentWidth);
  const projectedDepth = Math.max(0, baseDepth + depthDelta);

  // Clamp the projected depth based on surrounding nodes
  const nextNode = visibleNodes[overIndex + 1];
  const maxDepth = overNode.isGroup
    ? overNode.depth + 1
    : overNode.depth;
  const minDepth = nextNode ? nextNode.depth : 0;
  const clampedDepth = Math.min(Math.max(projectedDepth, minDepth), maxDepth);

  // Find the parent at the projected depth
  let parentId: string | null = null;
  if (clampedDepth > 0) {
    // Walk backwards from overNode to find a node at (clampedDepth - 1) that could be parent
    for (let i = overIndex; i >= 0; i--) {
      if (visibleNodes[i].depth === clampedDepth - 1) {
        parentId = visibleNodes[i].id;
        break;
      }
    }
  }

  return { depth: clampedDepth, parentId };
}

/**
 * Remove nodes by IDs (and all their descendants).
 */
export function removeNodes<T extends TreeNodeData>(
  flatNodes: FlatTreeNode<T>[],
  ids: string[]
): FlatTreeNode<T>[] {
  const removeSet = new Set(ids);

  // Also collect all descendants of removed nodes
  for (const node of flatNodes) {
    if (node.parentId !== null && removeSet.has(node.parentId)) {
      removeSet.add(node.id);
    }
  }

  const result = flatNodes.filter((n) => !removeSet.has(n.id));

  // Recompute sibling indices
  return reindexSiblings(result);
}

/**
 * Insert flat nodes into the tree at a specific position.
 */
export function insertNodes<T extends TreeNodeData>(
  flatNodes: FlatTreeNode<T>[],
  nodesToInsert: FlatTreeNode<T>[],
  targetParentId: string | null,
  targetIndex: number
): FlatTreeNode<T>[] {
  // Find the insertion point in the flat array
  let insertAt = flatNodes.length; // default: end

  // Find position of the target sibling
  let siblingCount = 0;
  for (let i = 0; i < flatNodes.length; i++) {
    if (flatNodes[i].parentId === targetParentId) {
      if (siblingCount === targetIndex) {
        insertAt = i;
        break;
      }
      siblingCount++;
    }
  }

  const result = [
    ...flatNodes.slice(0, insertAt),
    ...nodesToInsert,
    ...flatNodes.slice(insertAt),
  ];

  return reindexSiblings(result);
}

/**
 * Recompute sibling indices after structural changes.
 */
function reindexSiblings<T extends TreeNodeData>(
  flatNodes: FlatTreeNode<T>[]
): FlatTreeNode<T>[] {
  const indexCounters = new Map<string | null, number>();

  return flatNodes.map((node) => {
    const key = node.parentId;
    const currentIndex = indexCounters.get(key) ?? 0;
    indexCounters.set(key, currentIndex + 1);
    return { ...node, index: currentIndex };
  });
}
