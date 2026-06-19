"""message reply_to_id (quote-reply)

Revision ID: b7c2f1a9d3e4
Revises: 288a47108837
Create Date: 2026-06-19 12:00:00.000000

"""
from typing import Sequence, Union

import sqlalchemy as sa
from alembic import op

# revision identifiers, used by Alembic.
revision: str = "b7c2f1a9d3e4"
down_revision: Union[str, Sequence[str], None] = "288a47108837"
branch_labels: Union[str, Sequence[str], None] = None
depends_on: Union[str, Sequence[str], None] = None


def upgrade() -> None:
    """Upgrade schema."""
    with op.batch_alter_table("messages", schema=None) as batch_op:
        batch_op.add_column(sa.Column("reply_to_id", sa.Uuid(), nullable=True))
        batch_op.create_foreign_key(
            "fk_messages_reply_to_id",
            "messages",
            ["reply_to_id"],
            ["id"],
            ondelete="SET NULL",
        )


def downgrade() -> None:
    """Downgrade schema."""
    with op.batch_alter_table("messages", schema=None) as batch_op:
        batch_op.drop_constraint("fk_messages_reply_to_id", type_="foreignkey")
        batch_op.drop_column("reply_to_id")
