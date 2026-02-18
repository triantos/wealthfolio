use wealthfolio_core::goals::{Goal, GoalRepositoryTrait, GoalsAllocation, NewGoal};
use wealthfolio_core::Result;

use super::model::{GoalDB, GoalsAllocationDB, NewGoalDB};
use crate::db::{get_connection, WriteHandle};
use crate::errors::StorageError;
use crate::schema::goals;
use crate::schema::goals::dsl::*;
use crate::schema::goals_allocation;
use crate::sync::{write_outbox_event, OutboxWriteRequest};
use async_trait::async_trait;
use diesel::prelude::*;
use diesel::r2d2::{self, Pool};
use diesel::SqliteConnection;
use wealthfolio_core::sync::{SyncEntity, SyncOperation};

use std::sync::Arc;
use uuid::Uuid;

pub struct GoalRepository {
    pool: Arc<Pool<r2d2::ConnectionManager<SqliteConnection>>>,
    writer: WriteHandle,
}

impl GoalRepository {
    pub fn new(
        pool: Arc<Pool<r2d2::ConnectionManager<SqliteConnection>>>,
        writer: WriteHandle,
    ) -> Self {
        GoalRepository { pool, writer }
    }

    pub fn load_goals_impl(&self) -> Result<Vec<Goal>> {
        let mut conn = get_connection(&self.pool)?;
        let goals_db = goals
            .load::<GoalDB>(&mut conn)
            .map_err(StorageError::from)?;
        Ok(goals_db.into_iter().map(Goal::from).collect())
    }

    pub fn load_allocations_for_non_achieved_goals_impl(&self) -> Result<Vec<GoalsAllocation>> {
        let mut conn = get_connection(&self.pool)?;
        let allocations_db = goals_allocation::table
            .inner_join(goals::table.on(goals::id.eq(goals_allocation::goal_id)))
            .filter(goals::is_achieved.eq(false))
            .select(GoalsAllocationDB::as_select())
            .load::<GoalsAllocationDB>(&mut conn)
            .map_err(StorageError::from)?;
        Ok(allocations_db
            .into_iter()
            .map(GoalsAllocation::from)
            .collect())
    }
}

#[async_trait]
impl GoalRepositoryTrait for GoalRepository {
    fn load_goals(&self) -> Result<Vec<Goal>> {
        self.load_goals_impl()
    }

    async fn insert_new_goal(&self, new_goal: NewGoal) -> Result<Goal> {
        self.writer
            .exec(move |conn: &mut SqliteConnection| -> Result<Goal> {
                let mut new_goal_db: NewGoalDB = new_goal.into();
                new_goal_db.id = Some(Uuid::new_v4().to_string());

                let result_db = diesel::insert_into(goals::table)
                    .values(&new_goal_db)
                    .returning(GoalDB::as_returning())
                    .get_result(conn)
                    .map_err(StorageError::from)?;
                let payload_db = result_db.clone();
                let goal = Goal::from(result_db);
                write_outbox_event(
                    conn,
                    OutboxWriteRequest::new(
                        SyncEntity::Goal,
                        goal.id.clone(),
                        SyncOperation::Create,
                        serde_json::to_value(&payload_db)?,
                    ),
                )?;
                Ok(goal)
            })
            .await
    }

    async fn update_goal(&self, goal_update: Goal) -> Result<Goal> {
        let goal_id_owned = goal_update.id.clone();
        let goal_db: GoalDB = GoalDB {
            id: goal_update.id,
            title: goal_update.title,
            description: goal_update.description,
            target_amount: goal_update.target_amount,
            is_achieved: goal_update.is_achieved,
        };

        self.writer
            .exec(move |conn: &mut SqliteConnection| -> Result<Goal> {
                diesel::update(goals.find(goal_id_owned.clone()))
                    .set(&goal_db)
                    .execute(conn)
                    .map_err(StorageError::from)?;
                let result_db = goals
                    .filter(id.eq(goal_id_owned))
                    .first::<GoalDB>(conn)
                    .map_err(StorageError::from)?;
                let payload_db = result_db.clone();
                let goal = Goal::from(result_db);
                write_outbox_event(
                    conn,
                    OutboxWriteRequest::new(
                        SyncEntity::Goal,
                        goal.id.clone(),
                        SyncOperation::Update,
                        serde_json::to_value(&payload_db)?,
                    ),
                )?;
                Ok(goal)
            })
            .await
    }

    async fn delete_goal(&self, goal_id_to_delete: String) -> Result<usize> {
        let goal_id_for_event = goal_id_to_delete.clone();
        self.writer
            .exec(move |conn: &mut SqliteConnection| -> Result<usize> {
                let affected = diesel::delete(goals.find(goal_id_to_delete))
                    .execute(conn)
                    .map_err(StorageError::from)?;

                if affected > 0 {
                    write_outbox_event(
                        conn,
                        OutboxWriteRequest::new(
                            SyncEntity::Goal,
                            goal_id_for_event.clone(),
                            SyncOperation::Delete,
                            serde_json::json!({ "id": goal_id_for_event }),
                        ),
                    )?;
                }

                Ok(affected)
            })
            .await
    }

    fn load_allocations_for_non_achieved_goals(&self) -> Result<Vec<GoalsAllocation>> {
        self.load_allocations_for_non_achieved_goals_impl()
    }

    async fn upsert_goal_allocations(&self, allocations: Vec<GoalsAllocation>) -> Result<usize> {
        self.writer
            .exec(move |conn: &mut SqliteConnection| -> Result<usize> {
                let mut affected_rows = 0;
                for allocation in allocations {
                    let allocation_db: GoalsAllocationDB = allocation.into();
                    affected_rows += diesel::insert_into(goals_allocation::table)
                        .values(&allocation_db)
                        .on_conflict(goals_allocation::id)
                        .do_update()
                        .set(&allocation_db)
                        .execute(conn)
                        .map_err(StorageError::from)?;
                    write_outbox_event(
                        conn,
                        OutboxWriteRequest::new(
                            SyncEntity::GoalsAllocation,
                            allocation_db.id.clone(),
                            SyncOperation::Update,
                            serde_json::to_value(&allocation_db)?,
                        ),
                    )?;
                }
                Ok(affected_rows)
            })
            .await
    }
}
