-- AlterEnum
ALTER TYPE "role" ADD VALUE 'GUEST';

-- DropIndex
DROP INDEX "users_meta_idx";

-- AlterTable
ALTER TABLE "Post" ADD COLUMN     "rating" REAL NOT NULL DEFAULT 1.5;
