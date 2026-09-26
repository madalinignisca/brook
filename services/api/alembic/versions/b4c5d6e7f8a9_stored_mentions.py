"""stored mentions: message_mentions and messages.mention_everyone

Revision ID: b4c5d6e7f8a9
Revises: a3b4c5d6e7f8
Create Date: 2026-09-26 18:30:00.000000

"""
from typing import Sequence, Union

import sqlalchemy as sa
from alembic import op

# revision identifiers, used by Alembic.
revision: str = "b4c5d6e7f8a9"
down_revision: Union[str, Sequence[str], None] = "a3b4c5d6e7f8"
branch_labels: Union[str, Sequence[str], None] = None
depends_on: Union[str, Sequence[str], None] = None


def upgrade() -> None:
    """Upgrade schema."""
    # Existing messages get no stored mentions: resolving them now, against today's
    # members, would credit mentions that weren't true when they were sent.
    with op.batch_alter_table("messages", schema=None) as batch_op:
        batch_op.add_column(
            sa.Column("mention_everyone", sa.Boolean(), nullable=False, server_default=sa.false())
        )
    op.create_table(
        "message_mentions",
        sa.Column("message_id", sa.Uuid(), nullable=False),
        sa.Column("user_id", sa.Uuid(), nullable=False),
        sa.ForeignKeyConstraint(["message_id"], ["messages.id"], ondelete="CASCADE"),
        sa.ForeignKeyConstraint(["user_id"], ["users.id"], ondelete="CASCADE"),
        sa.PrimaryKeyConstraint("message_id", "user_id"),
    )
    op.create_index("ix_message_mentions_user_id", "message_mentions", ["user_id"])


def downgrade() -> None:
    """Downgrade schema."""
    op.drop_index("ix_message_mentions_user_id", table_name="message_mentions")
    op.drop_table("message_mentions")
    with op.batch_alter_table("messages", schema=None) as batch_op:
        batch_op.drop_column("mention_everyone")
