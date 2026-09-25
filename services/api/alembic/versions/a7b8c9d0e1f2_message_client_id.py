"""messages.client_id (outbox idempotency)

Revision ID: a7b8c9d0e1f2
Revises: f6a7b8c9d0e1
Create Date: 2026-09-25 15:00:00.000000

"""
from typing import Sequence, Union

import sqlalchemy as sa
from alembic import op

# revision identifiers, used by Alembic.
revision: str = "a7b8c9d0e1f2"
down_revision: Union[str, Sequence[str], None] = "f6a7b8c9d0e1"
branch_labels: Union[str, Sequence[str], None] = None
depends_on: Union[str, Sequence[str], None] = None


def upgrade() -> None:
    """Upgrade schema."""
    with op.batch_alter_table("messages", schema=None) as batch_op:
        batch_op.add_column(sa.Column("client_id", sa.Uuid(), nullable=True))
    # Partial: only messages sent with a client_id take part (older ones have none).
    op.create_index(
        "uq_messages_author_client_id",
        "messages",
        ["author_id", "client_id"],
        unique=True,
        postgresql_where=sa.text("client_id IS NOT NULL"),
        sqlite_where=sa.text("client_id IS NOT NULL"),
    )


def downgrade() -> None:
    """Downgrade schema."""
    op.drop_index("uq_messages_author_client_id", table_name="messages")
    with op.batch_alter_table("messages", schema=None) as batch_op:
        batch_op.drop_column("client_id")
