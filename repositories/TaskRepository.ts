import mongoose from "mongoose";

import Task, { type TaskDocument, type TaskInput } from "../models/Task";

export class TaskRepository {
  async findByHash(hash: string): Promise<TaskDocument | null> {
    return Task.findOne({ instructionHash: hash }).exec();
  }

  async createWithParent(
    newTask: TaskInput,
    parentId: string,
  ): Promise<TaskDocument> {
    const session = await mongoose.startSession();

    try {
      return await session.withTransaction(async () => {
        const [task] = await Task.create(
          [{ ...newTask, parentId }],
          { session },
        );
        const parent = await Task.updateOne(
          { _id: parentId },
          { $addToSet: { childIds: task._id } },
          { session },
        );

        if (parent.matchedCount !== 1) {
          throw new Error(`Parent task ${parentId} not found`);
        }

        return task;
      });
    } finally {
      await session.endSession();
    }
  }

  async addParentTask(
    task: TaskDocument,
    parentId: string,
  ): Promise<TaskDocument> {
    const session = await mongoose.startSession();

    try {
      return await session.withTransaction(async () => {
        const parent = await Task.updateOne(
          { _id: parentId },
          { $addToSet: { childIds: task._id } },
          { session },
        );

        if (parent.matchedCount !== 1) {
          throw new Error(`Parent task ${parentId} not found`);
        }

        task.parentId = parentId;
        return task.save({ session });
      });
    } finally {
      await session.endSession();
    }
  }
}

export default new TaskRepository();
