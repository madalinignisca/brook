"""owner_offers: pending offers to make a member a channel owner

Revision ID: a3b4c5d6e7f8
Revises: f2a3b4c5d6e7
Create Date: 2026-09-26 10:00:00.000000

"""
from typing import Sequence, Union

import sqlalchemy as sa
from alembic import op

# revision identifiers, used by Alembic.
revision: str = "a3b4c5d6e7f8"
down_revision: Union[str, Sequence[str], None] = "f2a3b4c5d6e7"
branch_labels: Union[str, Sequence[str], None] = None
depends_on: Union[str, Sequence[str], None] = None


def upgrade() -> None:
    """Upgrade schema."""
    op.create_table(
        "owner_offers",
        sa.Column("channel_id", sa.Uuid(), nullable=False),
        sa.Column("user_id", sa.Uuid(), nullable=False),
        sa.Column("offered_by", sa.Uuid(), nullable=False),
        sa.Column("created_at", sa.DateTime(timezone=True), nullable=False),
        sa.ForeignKeyConstraint(["channel_id"], ["channels.id"], ondelete="CASCADE"),
        sa.ForeignKeyConstraint(["user_id"], ["users.id"], ondelete="CASCADE"),
        sa.ForeignKeyConstraint(["offered_by"], ["users.id"], ondelete="CASCADE"),
        sa.PrimaryKeyConstraint("channel_id", "user_id"),
    )
    op.create_index("ix_owner_offers_offered_by", "owner_offers", ["offered_by"])


def downgrade() -> None:
    """Downgrade schema."""
    op.drop_index("ix_owner_offers_offered_by", table_name="owner_offers")
    op.drop_table("owner_offers")
