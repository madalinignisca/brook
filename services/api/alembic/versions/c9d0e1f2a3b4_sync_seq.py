"""sync: seq columns, counter, tombstones

Revision ID: c9d0e1f2a3b4
Revises: b8c9d0e1f2a3
Create Date: 2026-09-25 17:30:00.000000

"""
from typing import Sequence, Union

import sqlalchemy as sa
from alembic import op

# revision identifiers, used by Alembic.
revision: str = "c9d0e1f2a3b4"
down_revision: Union[str, Sequence[str], None] = "b8c9d0e1f2a3"
branch_labels: Union[str, Sequence[str], None] = None
depends_on: Union[str, Sequence[str], None] = None

_TABLES = ("users", "channels", "memberships", "messages")


def upgrade() -> None:
    """Upgrade schema."""
    # Backfill (sync spec §5): every existing row gets seq 1 and the counter starts at
    # 1, so any cursor > 0 from before this migration can't exist, and a first sync
    # (since=0, state only) sees everything current.
    for table in _TABLES:
        with op.batch_alter_table(table, schema=None) as batch_op:
            batch_op.add_column(
                sa.Column("seq", sa.BigInteger(), nullable=False, server_default="1")
            )
        op.create_index(f"ix_{table}_seq", table, ["seq"])
    op.create_table(
        "sync_counter",
        sa.Column("id", sa.Integer(), nullable=False),
        sa.Column("seq", sa.BigInteger(), nullable=False),
        sa.Column("floor", sa.BigInteger(), nullable=False),
        sa.PrimaryKeyConstraint("id"),
    )
    op.execute("INSERT INTO sync_counter (id, seq, floor) VALUES (1, 1, 0)")
    op.create_table(
        "sync_tombstones",
        sa.Column("id", sa.Uuid(), nullable=False),
        sa.Column("channel_id", sa.Uuid(), nullable=False),
        sa.Column("user_id", sa.Uuid(), nullable=False),
        sa.Column("seq", sa.BigInteger(), nullable=False),
        sa.PrimaryKeyConstraint("id"),
    )
    op.create_index("ix_sync_tombstones_channel_id", "sync_tombstones", ["channel_id"])
    op.create_index("ix_sync_tombstones_user_id", "sync_tombstones", ["user_id"])
    op.create_index("ix_sync_tombstones_seq", "sync_tombstones", ["seq"])


def downgrade() -> None:
    """Downgrade schema."""
    op.drop_table("sync_tombstones")
    op.drop_table("sync_counter")
    for table in _TABLES:
        op.drop_index(f"ix_{table}_seq", table_name=table)
        with op.batch_alter_table(table, schema=None) as batch_op:
            batch_op.drop_column("seq")
